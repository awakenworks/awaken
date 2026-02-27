//! LLM telemetry plugin aligned with OpenTelemetry GenAI Semantic Conventions.
//!
//! Captures per-inference and per-tool metrics via the Phase system,
//! forwarding them to a pluggable [`MetricsSink`].

use async_trait::async_trait;
use genai::chat::ChatOptions;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tirea_contract::runtime::plugin::agent::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::plugin::phase::effect::PhaseOutput;
use tirea_contract::runtime::plugin::phase::{
    AfterInferenceContext, AfterToolExecuteContext, BeforeInferenceContext,
    BeforeToolExecuteContext, PluginPhaseContext, RunEndContext, RunStartContext,
};
use tirea_contract::runtime::plugin::AgentPlugin;
use tirea_contract::runtime::run::{InferenceError, InferenceErrorState};
use tirea_contract::TokenUsage;

fn lock_unpoison<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

// =============================================================================
// Span types (OTel GenAI aligned)
// =============================================================================

/// A single LLM inference span.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenAISpan {
    /// Model identifier (e.g. "gpt-4o-mini"). OTel: `gen_ai.request.model`.
    pub model: String,
    /// Provider name (e.g. "openai"). OTel: `gen_ai.provider.name`.
    pub provider: String,
    /// Operation name (e.g. "chat"). OTel: `gen_ai.operation.name`.
    pub operation: String,
    /// Response model (may differ from request model). OTel: `gen_ai.response.model`.
    pub response_model: Option<String>,
    /// Response ID. OTel: `gen_ai.response.id`.
    pub response_id: Option<String>,
    /// Finish reasons. OTel: `gen_ai.response.finish_reasons`.
    pub finish_reasons: Vec<String>,
    /// Error type if the inference failed. OTel: `error.type`.
    pub error_type: Option<String>,
    /// Input (prompt) tokens. OTel: `gen_ai.usage.input_tokens`.
    pub input_tokens: Option<i32>,
    /// Output (completion) tokens. OTel: `gen_ai.usage.output_tokens`.
    pub output_tokens: Option<i32>,
    /// Total tokens (non-OTel convenience).
    pub total_tokens: Option<i32>,
    /// Cache-read input tokens. OTel: `gen_ai.usage.cache_read.input_tokens`.
    pub cache_read_input_tokens: Option<i32>,
    /// Cache-creation input tokens. OTel: `gen_ai.usage.cache_creation.input_tokens`.
    pub cache_creation_input_tokens: Option<i32>,
    /// Sampling temperature. OTel: `gen_ai.request.temperature`.
    pub temperature: Option<f64>,
    /// Nucleus sampling (top-p). OTel: `gen_ai.request.top_p`.
    pub top_p: Option<f64>,
    /// Maximum tokens to generate. OTel: `gen_ai.request.max_tokens`.
    pub max_tokens: Option<u32>,
    /// Stop sequences. OTel: `gen_ai.request.stop_sequences`.
    pub stop_sequences: Vec<String>,
    /// Wall-clock duration in milliseconds. OTel: `gen_ai.client.operation.duration`.
    pub duration_ms: u64,
}

/// A single tool execution span.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpan {
    /// Tool name. OTel: `gen_ai.tool.name`.
    pub name: String,
    /// Operation name (always "execute_tool"). OTel: `gen_ai.operation.name`.
    pub operation: String,
    /// Tool call ID. OTel: `gen_ai.tool.call.id`.
    pub call_id: String,
    /// Tool type (always "function"). OTel: `gen_ai.tool.type`.
    pub tool_type: String,
    /// Error type when the tool failed. OTel: `error.type`.
    pub error_type: Option<String>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

impl ToolSpan {
    /// Whether the tool execution succeeded (no error).
    pub fn is_success(&self) -> bool {
        self.error_type.is_none()
    }
}

/// Per-model aggregated inference statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelStats {
    /// Model identifier (e.g. "claude-sonnet-4-5").
    pub model: String,
    /// Provider name (e.g. "anthropic").
    pub provider: String,
    /// Number of inference calls.
    pub inference_count: usize,
    /// Total input (prompt) tokens.
    pub input_tokens: i32,
    /// Total output (completion) tokens.
    pub output_tokens: i32,
    /// Total tokens (input + output).
    pub total_tokens: i32,
    /// Total cache-read input tokens.
    pub cache_read_input_tokens: i32,
    /// Total cache-creation input tokens.
    pub cache_creation_input_tokens: i32,
    /// Total wall-clock inference duration in milliseconds.
    pub total_duration_ms: u64,
}

/// Per-tool aggregated execution statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolStats {
    /// Tool name.
    pub name: String,
    /// Number of calls.
    pub call_count: usize,
    /// Number of failed calls.
    pub failure_count: usize,
    /// Total wall-clock execution duration in milliseconds.
    pub total_duration_ms: u64,
}

/// Aggregated metrics for an agent session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentMetrics {
    /// All inference spans.
    pub inferences: Vec<GenAISpan>,
    /// All tool spans.
    pub tools: Vec<ToolSpan>,
    /// Total session duration in milliseconds.
    pub session_duration_ms: u64,
}

impl AgentMetrics {
    /// Total input tokens across all inferences.
    pub fn total_input_tokens(&self) -> i32 {
        self.inferences.iter().filter_map(|s| s.input_tokens).sum()
    }

    /// Total output tokens across all inferences.
    pub fn total_output_tokens(&self) -> i32 {
        self.inferences.iter().filter_map(|s| s.output_tokens).sum()
    }

    /// Total tokens across all inferences.
    pub fn total_tokens(&self) -> i32 {
        self.inferences.iter().filter_map(|s| s.total_tokens).sum()
    }

    /// Total cache-read input tokens across all inferences.
    pub fn total_cache_read_tokens(&self) -> i32 {
        self.inferences
            .iter()
            .filter_map(|s| s.cache_read_input_tokens)
            .sum()
    }

    /// Total cache-creation input tokens across all inferences.
    pub fn total_cache_creation_tokens(&self) -> i32 {
        self.inferences
            .iter()
            .filter_map(|s| s.cache_creation_input_tokens)
            .sum()
    }

    /// Total wall-clock inference duration in milliseconds.
    pub fn total_inference_duration_ms(&self) -> u64 {
        self.inferences.iter().map(|s| s.duration_ms).sum()
    }

    /// Total wall-clock tool execution duration in milliseconds.
    pub fn total_tool_duration_ms(&self) -> u64 {
        self.tools.iter().map(|s| s.duration_ms).sum()
    }

    /// Number of inferences.
    pub fn inference_count(&self) -> usize {
        self.inferences.len()
    }

    /// Number of tool executions.
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Number of failed tool executions.
    pub fn tool_failures(&self) -> usize {
        self.tools.iter().filter(|t| !t.is_success()).count()
    }

    /// Inference statistics grouped by `(model, provider)`.
    ///
    /// Results are sorted by model name for deterministic output.
    pub fn stats_by_model(&self) -> Vec<ModelStats> {
        let mut map: HashMap<(String, String), ModelStats> = HashMap::new();
        for span in &self.inferences {
            let key = (span.model.clone(), span.provider.clone());
            let entry = map.entry(key).or_insert_with(|| ModelStats {
                model: span.model.clone(),
                provider: span.provider.clone(),
                ..Default::default()
            });
            entry.inference_count += 1;
            entry.input_tokens += span.input_tokens.unwrap_or(0);
            entry.output_tokens += span.output_tokens.unwrap_or(0);
            entry.total_tokens += span.total_tokens.unwrap_or(0);
            entry.cache_read_input_tokens += span.cache_read_input_tokens.unwrap_or(0);
            entry.cache_creation_input_tokens += span.cache_creation_input_tokens.unwrap_or(0);
            entry.total_duration_ms += span.duration_ms;
        }
        let mut result: Vec<ModelStats> = map.into_values().collect();
        result.sort_by(|a, b| a.model.cmp(&b.model));
        result
    }

    /// Tool execution statistics grouped by tool name.
    ///
    /// Results are sorted by tool name for deterministic output.
    pub fn stats_by_tool(&self) -> Vec<ToolStats> {
        let mut map: HashMap<String, ToolStats> = HashMap::new();
        for span in &self.tools {
            let entry = map.entry(span.name.clone()).or_insert_with(|| ToolStats {
                name: span.name.clone(),
                ..Default::default()
            });
            entry.call_count += 1;
            if !span.is_success() {
                entry.failure_count += 1;
            }
            entry.total_duration_ms += span.duration_ms;
        }
        let mut result: Vec<ToolStats> = map.into_values().collect();
        result.sort_by(|a, b| a.name.cmp(&b.name));
        result
    }
}

// =============================================================================
// MetricsSink trait
// =============================================================================

/// Trait for consuming telemetry data.
///
/// Implementations can send data to OTel collectors, log files, etc.
pub trait MetricsSink: Send + Sync {
    /// Called when an inference completes.
    fn on_inference(&self, span: &GenAISpan);

    /// Called when a tool execution completes.
    fn on_tool(&self, span: &ToolSpan);

    /// Called when a session ends with aggregated metrics.
    fn on_run_end(&self, metrics: &AgentMetrics);
}

// =============================================================================
// InMemorySink
// =============================================================================

/// In-memory sink for testing and inspection.
#[derive(Debug, Clone, Default)]
pub struct InMemorySink {
    inner: Arc<Mutex<AgentMetrics>>,
}

impl InMemorySink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a snapshot of the current metrics.
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
        let mut inner = self.inner.lock().unwrap();
        inner.session_duration_ms = metrics.session_duration_ms;
    }
}

// =============================================================================
// LLMMetryPlugin
// =============================================================================

/// Plugin that captures LLM and tool telemetry.
///
/// Attach an [`InMemorySink`] (or custom sink) to collect metrics:
///
/// ```ignore
/// let sink = InMemorySink::new();
/// let plugin = LLMMetryPlugin::new(sink.clone())
///     .with_model("gpt-4o-mini")
///     .with_provider("openai");
/// let config = AgentDefinition::new("gpt-4o-mini")
///     .with_plugin(Arc::new(plugin));
/// // ... run agent ...
/// let metrics = sink.metrics();
/// let _total = metrics.total_tokens();
/// ```
pub struct LLMMetryPlugin {
    sink: Arc<dyn MetricsSink>,
    run_start: Mutex<Option<Instant>>,
    metrics: Mutex<AgentMetrics>,
    /// Inference timing: set at BeforeInference, consumed at AfterInference.
    inference_start: Mutex<Option<Instant>>,
    /// Tool timing per call_id: set at BeforeToolExecute, consumed at AfterToolExecute.
    tool_start: Mutex<HashMap<String, Instant>>,
    /// Model name captured from AgentDefinition (set externally or from data).
    model: Mutex<String>,
    /// Provider name (e.g. "openai", "anthropic"). OTel: `gen_ai.provider.name`.
    provider: Mutex<String>,
    /// Operation name (defaults to "chat").
    operation: String,
    /// Sampling temperature. OTel: `gen_ai.request.temperature`.
    temperature: Mutex<Option<f64>>,
    /// Nucleus sampling (top-p). OTel: `gen_ai.request.top_p`.
    top_p: Mutex<Option<f64>>,
    /// Maximum tokens to generate. OTel: `gen_ai.request.max_tokens`.
    max_tokens: Mutex<Option<u32>>,
    /// Stop sequences. OTel: `gen_ai.request.stop_sequences`.
    stop_sequences: Mutex<Vec<String>>,
    /// Tracing span for the current inference (created at BeforeInference, closed at AfterInference).
    inference_tracing_span: Mutex<Option<tracing::Span>>,
    /// Tracing span per tool call_id (created at BeforeToolExecute, closed at AfterToolExecute).
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
impl AgentPlugin for LLMMetryPlugin {
    fn id(&self) -> &str {
        "llmmetry"
    }

    async fn run_start(&self, _ctx: &mut RunStartContext<'_, '_>) {
        *lock_unpoison(&self.run_start) = Some(Instant::now());
    }

    async fn before_inference(&self, _ctx: &mut BeforeInferenceContext<'_, '_>) {
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
    }

    async fn after_inference(&self, ctx: &mut AfterInferenceContext<'_, '_>) {
        let duration_ms = self
            .inference_start
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(0);

        let usage = ctx.response_opt().and_then(|r| r.usage.as_ref());
        let (input_tokens, output_tokens, total_tokens) = extract_token_counts(usage);
        let (cache_read_input_tokens, cache_creation_input_tokens) = extract_cache_tokens(usage);
        let error = inference_error_from_state(ctx);

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
    }

    async fn before_tool_execute(&self, ctx: &mut BeforeToolExecuteContext<'_, '_>) {
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
    }

    async fn after_tool_execute(&self, ctx: &mut AfterToolExecuteContext<'_, '_>) {
        let call_id_for_span = ctx.tool_call_id().unwrap_or_default().to_string();
        let duration_ms = self
            .tool_start
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&call_id_for_span)
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(0);

        let result = ctx.tool_result();
        let error_type = if result.status == tirea_contract::runtime::tool_call::ToolStatus::Error {
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
    }

    async fn run_end(&self, _ctx: &mut RunEndContext<'_, '_>) {
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
    }
}

#[async_trait]
impl AgentBehavior for LLMMetryPlugin {
    fn id(&self) -> &str {
        "llmmetry"
    }

    async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        *lock_unpoison(&self.run_start) = Some(Instant::now());
        PhaseOutput::default()
    }

    async fn before_inference(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
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
        PhaseOutput::default()
    }

    async fn after_inference(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
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
        let error = inference_error_from_snapshot(ctx);

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
        PhaseOutput::default()
    }

    async fn before_tool_execute(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
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
        PhaseOutput::default()
    }

    async fn after_tool_execute(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let call_id_for_span = ctx.tool_call_id().unwrap_or_default().to_string();
        let duration_ms = self
            .tool_start
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&call_id_for_span)
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(0);

        let Some(result) = ctx.tool_result() else {
            return PhaseOutput::default();
        };
        let error_type =
            if result.status == tirea_contract::runtime::tool_call::ToolStatus::Error {
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
        PhaseOutput::default()
    }

    async fn run_end(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
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
        PhaseOutput::default()
    }
}

fn extract_token_counts(usage: Option<&TokenUsage>) -> (Option<i32>, Option<i32>, Option<i32>) {
    match usage {
        Some(u) => (u.prompt_tokens, u.completion_tokens, u.total_tokens),
        None => (None, None, None),
    }
}

fn extract_cache_tokens(usage: Option<&TokenUsage>) -> (Option<i32>, Option<i32>) {
    match usage {
        Some(u) => (u.cache_read_tokens, u.cache_creation_tokens),
        None => (None, None),
    }
}

fn inference_error_from_state(ctx: &impl PluginPhaseContext) -> Option<InferenceError> {
    let state = ctx.state_of::<InferenceErrorState>();
    state.error().ok().flatten()
}

fn inference_error_from_snapshot(ctx: &ReadOnlyContext<'_>) -> Option<InferenceError> {
    ctx.snapshot_of::<InferenceErrorState>()
        .ok()
        .and_then(|s| s.error)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::join_all;
    use serde_json::json;
    use std::sync::Arc;
    use tirea_contract::runtime::plugin::phase::{
        AfterInferenceContext, AfterToolExecuteContext, BeforeInferenceContext,
        BeforeToolExecuteContext, Phase, RunEndContext, RunStartContext, StepContext,
        StepEndContext, StepStartContext, ToolContext as PhaseToolContext,
    };
    use tirea_contract::runtime::plugin::AgentPlugin;
    use tirea_contract::runtime::StreamResult;
    use tirea_contract::testing::TestFixture;
    use tirea_contract::thread::ToolCall;
    use tirea_contract::runtime::tool_call::ToolResult;

    #[async_trait::async_trait]
    trait AgentPluginTestDispatch {
        async fn run_phase(&self, phase: Phase, step: &mut StepContext<'_>);
    }

    #[async_trait::async_trait]
    impl<T> AgentPluginTestDispatch for T
    where
        T: AgentPlugin + ?Sized,
    {
        async fn run_phase(&self, phase: Phase, step: &mut StepContext<'_>) {
            match phase {
                Phase::RunStart => {
                    let mut ctx = RunStartContext::new(step);
                    self.run_start(&mut ctx).await;
                }
                Phase::StepStart => {
                    let mut ctx = StepStartContext::new(step);
                    self.step_start(&mut ctx).await;
                }
                Phase::BeforeInference => {
                    let mut ctx = BeforeInferenceContext::new(step);
                    self.before_inference(&mut ctx).await;
                }
                Phase::AfterInference => {
                    let mut ctx = AfterInferenceContext::new(step);
                    self.after_inference(&mut ctx).await;
                }
                Phase::BeforeToolExecute => {
                    let mut ctx = BeforeToolExecuteContext::new(step);
                    self.before_tool_execute(&mut ctx).await;
                }
                Phase::AfterToolExecute => {
                    let mut ctx = AfterToolExecuteContext::new(step);
                    self.after_tool_execute(&mut ctx).await;
                }
                Phase::StepEnd => {
                    let mut ctx = StepEndContext::new(step);
                    self.step_end(&mut ctx).await;
                }
                Phase::RunEnd => {
                    let mut ctx = RunEndContext::new(step);
                    self.run_end(&mut ctx).await;
                }
            }
        }
    }

    fn usage(prompt: i32, completion: i32, total: i32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            total_tokens: Some(total),
            cache_read_tokens: None,
            cache_creation_tokens: None,
        }
    }

    fn usage_with_cache(prompt: i32, completion: i32, total: i32, cached: i32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            total_tokens: Some(total),
            cache_read_tokens: Some(cached),
            cache_creation_tokens: None,
        }
    }

    fn make_span(model: &str, provider: &str) -> GenAISpan {
        GenAISpan {
            model: model.into(),
            provider: provider.into(),
            operation: "chat".into(),
            response_model: None,
            response_id: None,
            finish_reasons: Vec::new(),
            error_type: None,
            input_tokens: Some(10),
            output_tokens: Some(20),
            total_tokens: Some(30),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: Vec::new(),
            duration_ms: 100,
        }
    }

    fn make_tool_span(name: &str, call_id: &str) -> ToolSpan {
        ToolSpan {
            name: name.into(),
            operation: "execute_tool".into(),
            call_id: call_id.into(),
            tool_type: "function".into(),
            error_type: None,
            duration_ms: 10,
        }
    }

    // ---- ToolSpan::is_success ----

    #[test]
    fn test_tool_span_is_success() {
        let span = make_tool_span("search", "c1");
        assert!(span.is_success());

        let span = ToolSpan {
            error_type: Some("permission denied".into()),
            ..make_tool_span("write", "c2")
        };
        assert!(!span.is_success());
    }

    // ---- AgentMetrics ----

    #[test]
    fn test_agent_metrics_defaults() {
        let m = AgentMetrics::default();
        assert_eq!(m.total_input_tokens(), 0);
        assert_eq!(m.total_output_tokens(), 0);
        assert_eq!(m.total_tokens(), 0);
        assert_eq!(m.inference_count(), 0);
        assert_eq!(m.tool_count(), 0);
        assert_eq!(m.tool_failures(), 0);
    }

    #[test]
    fn test_agent_metrics_aggregation() {
        let m = AgentMetrics {
            inferences: vec![
                make_span("m", "openai"),
                GenAISpan {
                    input_tokens: Some(5),
                    output_tokens: None,
                    total_tokens: Some(8),
                    duration_ms: 50,
                    ..make_span("m", "openai")
                },
            ],
            tools: vec![
                make_tool_span("a", "c1"),
                ToolSpan {
                    error_type: Some("permission denied".into()),
                    ..make_tool_span("b", "c2")
                },
            ],
            session_duration_ms: 500,
        };
        assert_eq!(m.total_input_tokens(), 15);
        assert_eq!(m.total_output_tokens(), 20);
        assert_eq!(m.total_tokens(), 38);
        assert_eq!(m.inference_count(), 2);
        assert_eq!(m.tool_count(), 2);
        assert_eq!(m.tool_failures(), 1);
    }

    // ---- InMemorySink ----

    #[test]
    fn test_in_memory_sink_collects() {
        let sink = InMemorySink::new();
        sink.on_inference(&make_span("test", "openai"));
        sink.on_tool(&make_tool_span("t", "c1"));
        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.tool_count(), 1);
    }

    #[test]
    fn test_in_memory_sink_run_end() {
        let sink = InMemorySink::new();
        let metrics = AgentMetrics {
            session_duration_ms: 999,
            ..Default::default()
        };
        sink.on_run_end(&metrics);
        assert_eq!(sink.metrics().session_duration_ms, 999);
    }

    // ---- LLMMetryPlugin ----

    #[tokio::test]
    async fn test_plugin_captures_inference() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("openai");

        let mut step = fix.step(vec![]);

        plugin.run_phase(Phase::BeforeInference, &mut step).await;

        step.response = Some(StreamResult {
            text: "hello".into(),
            tool_calls: vec![],
            usage: Some(usage(100, 50, 150)),
        });

        plugin.run_phase(Phase::AfterInference, &mut step).await;

        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.total_input_tokens(), 100);
        assert_eq!(m.total_output_tokens(), 50);
        assert_eq!(m.inferences[0].model, "gpt-4");
        assert_eq!(m.inferences[0].provider, "openai");
        assert_eq!(m.inferences[0].operation, "chat");
        assert!(m.inferences[0].cache_read_input_tokens.is_none());
    }

    #[tokio::test]
    async fn test_plugin_captures_inference_with_cache() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("openai");

        let mut step = fix.step(vec![]);

        plugin.run_phase(Phase::BeforeInference, &mut step).await;

        step.response = Some(StreamResult {
            text: "hello".into(),
            tool_calls: vec![],
            usage: Some(usage_with_cache(100, 50, 150, 30)),
        });

        plugin.run_phase(Phase::AfterInference, &mut step).await;

        let m = sink.metrics();
        let span = &m.inferences[0];
        assert_eq!(span.cache_read_input_tokens, Some(30));
        assert!(span.cache_creation_input_tokens.is_none());
    }

    #[tokio::test]
    async fn test_plugin_captures_tool() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone());

        let mut step = fix.step(vec![]);

        let call = ToolCall::new("c1", "search", json!({}));
        step.tool = Some(PhaseToolContext::new(&call));

        plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

        step.tool.as_mut().unwrap().result =
            Some(ToolResult::success("search", json!({"found": true})));

        plugin.run_phase(Phase::AfterToolExecute, &mut step).await;

        let m = sink.metrics();
        assert_eq!(m.tool_count(), 1);
        assert!(m.tools[0].is_success());
        assert_eq!(m.tools[0].name, "search");
        assert_eq!(m.tools[0].call_id, "c1");
        assert_eq!(m.tools[0].operation, "execute_tool");
        assert!(m.tools[0].error_type.is_none());
    }

    #[tokio::test]
    async fn test_plugin_captures_tool_failure() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone());

        let mut step = fix.step(vec![]);

        let call = ToolCall::new("c1", "write", json!({}));
        step.tool = Some(PhaseToolContext::new(&call));

        plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

        step.tool.as_mut().unwrap().result = Some(ToolResult::error("write", "permission denied"));

        plugin.run_phase(Phase::AfterToolExecute, &mut step).await;

        let m = sink.metrics();
        assert!(!m.tools[0].is_success());
        assert_eq!(m.tools[0].error_type.as_deref(), Some("tool_error"));
    }

    #[tokio::test]
    async fn test_plugin_session_lifecycle() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone());

        let mut step = fix.step(vec![]);

        plugin.run_phase(Phase::RunStart, &mut step).await;

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        plugin.run_phase(Phase::RunEnd, &mut step).await;

        let m = sink.metrics();
        assert!(m.session_duration_ms >= 10);
    }

    #[tokio::test]
    async fn test_plugin_no_usage() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone()).with_model("m");

        let mut step = fix.step(vec![]);

        plugin.run_phase(Phase::BeforeInference, &mut step).await;
        step.response = Some(StreamResult {
            text: "hi".into(),
            tool_calls: vec![],
            usage: None,
        });
        plugin.run_phase(Phase::AfterInference, &mut step).await;

        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert!(m.inferences[0].input_tokens.is_none());
        assert!(m.inferences[0].cache_read_input_tokens.is_none());
    }

    #[tokio::test]
    async fn test_plugin_multiple_rounds() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone()).with_model("m");

        let mut step = fix.step(vec![]);

        for i in 0..3 {
            plugin.run_phase(Phase::BeforeInference, &mut step).await;
            step.response = Some(StreamResult {
                text: format!("r{i}"),
                tool_calls: vec![],
                usage: Some(usage(10 * (i + 1), 5 * (i + 1), 15 * (i + 1))),
            });
            plugin.run_phase(Phase::AfterInference, &mut step).await;
        }

        let m = sink.metrics();
        assert_eq!(m.inference_count(), 3);
        assert_eq!(m.total_input_tokens(), 60); // 10+20+30
        assert_eq!(m.total_output_tokens(), 30); // 5+10+15
    }

    #[tokio::test]
    async fn test_plugin_captures_inference_error() {
        let fix = TestFixture::new_with_state(json!({
            "__inference_error": {
                "error": {
                    "type": "rate_limited",
                    "message": "429"
                }
            }
        }));
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("openai");

        let mut step = fix.step(vec![]);

        plugin.run_phase(Phase::BeforeInference, &mut step).await;
        plugin.run_phase(Phase::AfterInference, &mut step).await;

        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.inferences[0].error_type.as_deref(), Some("rate_limited"));
    }

    #[tokio::test]
    async fn test_plugin_parallel_tool_spans_are_isolated_by_call_id() {
        let fix = TestFixture::new();
        use std::time::Duration;

        let sink = InMemorySink::new();
        let plugin = Arc::new(LLMMetryPlugin::new(sink.clone()).with_provider("p"));

        let calls = vec![
            ToolCall::new("c1", "search", json!({"q": "a"})),
            ToolCall::new("c2", "write", json!({"path": "x"})),
            ToolCall::new("c3", "read", json!({"path": "y"})),
        ];

        let fix = &fix;
        let tasks = calls.into_iter().enumerate().map(|(i, call)| {
            let plugin = plugin.clone();
            async move {
                let mut step = fix.step(vec![]);
                step.tool = Some(PhaseToolContext::new(&call));
                plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
                // Stagger completion to maximize the chance of cross-talk.
                tokio::time::sleep(Duration::from_millis(5 * (3 - i) as u64)).await;
                step.tool.as_mut().unwrap().result =
                    Some(ToolResult::success(&call.name, json!({"ok": true})));
                plugin.run_phase(Phase::AfterToolExecute, &mut step).await;
            }
        });

        join_all(tasks).await;

        let m = sink.metrics();
        assert_eq!(m.tool_count(), 3);
        let mut ids: Vec<String> = m.tools.iter().map(|t| t.call_id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["c1", "c2", "c3"]);
    }

    #[test]
    fn test_genai_span_serialization() {
        let span = make_span("gpt-4", "openai");
        let json = serde_json::to_value(&span).unwrap();
        assert_eq!(json["model"], "gpt-4");
        assert_eq!(json["input_tokens"], 10);
        assert_eq!(json["provider"], "openai");
        assert_eq!(json["operation"], "chat");
    }

    #[test]
    fn test_tool_span_serialization() {
        let span = make_tool_span("search", "c1");
        let json = serde_json::to_value(&span).unwrap();
        assert_eq!(json["name"], "search");
        assert_eq!(json["call_id"], "c1");
        assert_eq!(json["operation"], "execute_tool");
    }

    #[test]
    fn test_agent_metrics_serialization() {
        let m = AgentMetrics::default();
        let json = serde_json::to_string(&m).unwrap();
        let m2: AgentMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(m2.session_duration_ms, 0);
    }

    #[test]
    fn test_extract_token_counts_some() {
        let u = usage(10, 20, 30);
        let (i, o, t) = extract_token_counts(Some(&u));
        assert_eq!(i, Some(10));
        assert_eq!(o, Some(20));
        assert_eq!(t, Some(30));
    }

    #[test]
    fn test_extract_token_counts_none() {
        let (i, o, t) = extract_token_counts(None);
        assert!(i.is_none());
        assert!(o.is_none());
        assert!(t.is_none());
    }

    #[test]
    fn test_extract_cache_tokens() {
        let u = usage_with_cache(100, 50, 150, 30);
        let (read, creation) = extract_cache_tokens(Some(&u));
        assert_eq!(read, Some(30));
        assert!(creation.is_none());
    }

    #[test]
    fn test_extract_cache_tokens_none() {
        assert_eq!(extract_cache_tokens(None), (None, None));
        let u = usage(10, 20, 30);
        assert_eq!(extract_cache_tokens(Some(&u)), (None, None));
    }

    // ---- stats_by_model ----

    #[test]
    fn test_stats_by_model_empty() {
        let m = AgentMetrics::default();
        assert!(m.stats_by_model().is_empty());
    }

    #[test]
    fn test_stats_by_model_single() {
        let m = AgentMetrics {
            inferences: vec![
                make_span("gpt-4", "openai"),
                GenAISpan {
                    input_tokens: Some(5),
                    output_tokens: Some(3),
                    total_tokens: Some(8),
                    duration_ms: 50,
                    ..make_span("gpt-4", "openai")
                },
            ],
            ..Default::default()
        };
        let stats = m.stats_by_model();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].model, "gpt-4");
        assert_eq!(stats[0].provider, "openai");
        assert_eq!(stats[0].inference_count, 2);
        assert_eq!(stats[0].input_tokens, 15);
        assert_eq!(stats[0].output_tokens, 23);
        assert_eq!(stats[0].total_tokens, 38);
        assert_eq!(stats[0].total_duration_ms, 150);
    }

    #[test]
    fn test_stats_by_model_multiple() {
        let m = AgentMetrics {
            inferences: vec![
                make_span("gpt-4", "openai"),
                make_span("claude-3", "anthropic"),
                GenAISpan {
                    input_tokens: Some(50),
                    output_tokens: Some(25),
                    total_tokens: Some(75),
                    duration_ms: 200,
                    ..make_span("claude-3", "anthropic")
                },
            ],
            ..Default::default()
        };
        let stats = m.stats_by_model();
        assert_eq!(stats.len(), 2);
        // Sorted by model name
        assert_eq!(stats[0].model, "claude-3");
        assert_eq!(stats[0].inference_count, 2);
        assert_eq!(stats[0].input_tokens, 60);
        assert_eq!(stats[0].output_tokens, 45);
        assert_eq!(stats[0].total_duration_ms, 300);

        assert_eq!(stats[1].model, "gpt-4");
        assert_eq!(stats[1].inference_count, 1);
    }

    #[test]
    fn test_stats_by_model_with_cache_tokens() {
        let m = AgentMetrics {
            inferences: vec![GenAISpan {
                cache_read_input_tokens: Some(30),
                cache_creation_input_tokens: Some(10),
                ..make_span("claude-3", "anthropic")
            }],
            ..Default::default()
        };
        let stats = m.stats_by_model();
        assert_eq!(stats[0].cache_read_input_tokens, 30);
        assert_eq!(stats[0].cache_creation_input_tokens, 10);
    }

    // ---- stats_by_tool ----

    #[test]
    fn test_stats_by_tool_empty() {
        let m = AgentMetrics::default();
        assert!(m.stats_by_tool().is_empty());
    }

    #[test]
    fn test_stats_by_tool_single() {
        let m = AgentMetrics {
            tools: vec![
                make_tool_span("search", "c1"),
                ToolSpan {
                    duration_ms: 20,
                    ..make_tool_span("search", "c2")
                },
            ],
            ..Default::default()
        };
        let stats = m.stats_by_tool();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].name, "search");
        assert_eq!(stats[0].call_count, 2);
        assert_eq!(stats[0].failure_count, 0);
        assert_eq!(stats[0].total_duration_ms, 30);
    }

    #[test]
    fn test_stats_by_tool_multiple() {
        let m = AgentMetrics {
            tools: vec![
                make_tool_span("search", "c1"),
                make_tool_span("write", "c2"),
                make_tool_span("search", "c3"),
            ],
            ..Default::default()
        };
        let stats = m.stats_by_tool();
        assert_eq!(stats.len(), 2);
        // Sorted by name
        assert_eq!(stats[0].name, "search");
        assert_eq!(stats[0].call_count, 2);
        assert_eq!(stats[1].name, "write");
        assert_eq!(stats[1].call_count, 1);
    }

    #[test]
    fn test_stats_by_tool_with_failures() {
        let m = AgentMetrics {
            tools: vec![
                make_tool_span("write", "c1"),
                ToolSpan {
                    error_type: Some("permission denied".into()),
                    ..make_tool_span("write", "c2")
                },
                ToolSpan {
                    error_type: Some("not found".into()),
                    ..make_tool_span("write", "c3")
                },
            ],
            ..Default::default()
        };
        let stats = m.stats_by_tool();
        assert_eq!(stats[0].call_count, 3);
        assert_eq!(stats[0].failure_count, 2);
    }

    // ---- total cache/duration methods ----

    #[test]
    fn test_total_cache_tokens() {
        let m = AgentMetrics {
            inferences: vec![
                GenAISpan {
                    cache_read_input_tokens: Some(30),
                    cache_creation_input_tokens: Some(10),
                    ..make_span("m", "p")
                },
                GenAISpan {
                    cache_read_input_tokens: Some(20),
                    cache_creation_input_tokens: None,
                    ..make_span("m", "p")
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.total_cache_read_tokens(), 50);
        assert_eq!(m.total_cache_creation_tokens(), 10);
    }

    #[test]
    fn test_total_duration_methods() {
        let m = AgentMetrics {
            inferences: vec![
                make_span("m", "p"), // 100ms
                GenAISpan {
                    duration_ms: 200,
                    ..make_span("m", "p")
                },
            ],
            tools: vec![
                make_tool_span("a", "c1"), // 10ms
                ToolSpan {
                    duration_ms: 30,
                    ..make_tool_span("b", "c2")
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.total_inference_duration_ms(), 300);
        assert_eq!(m.total_tool_duration_ms(), 40);
    }

    // ---- Tracing span capture tests ----

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::registry::LookupSpan;

    #[derive(Debug, Clone)]
    struct CapturedSpan {
        name: String,
        was_closed: bool,
    }

    struct SpanCaptureLayer {
        captured: Arc<Mutex<Vec<CapturedSpan>>>,
    }

    impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> tracing_subscriber::Layer<S>
        for SpanCaptureLayer
    {
        fn on_new_span(
            &self,
            _attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if let Some(span_ref) = ctx.span(id) {
                self.captured.lock().unwrap().push(CapturedSpan {
                    name: span_ref.name().to_string(),
                    was_closed: false,
                });
            }
        }

        fn on_close(&self, id: tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
            if let Some(span_ref) = ctx.span(&id) {
                let name = span_ref.name().to_string();
                let mut captured = self.captured.lock().unwrap();
                if let Some(entry) = captured.iter_mut().find(|c| c.name == name) {
                    entry.was_closed = true;
                }
            }
        }
    }

    #[tokio::test]
    async fn test_tracing_span_inference() {
        let fix = TestFixture::new();
        let captured = Arc::new(Mutex::new(Vec::<CapturedSpan>::new()));
        let layer = SpanCaptureLayer {
            captured: captured.clone(),
        };
        let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone())
            .with_model("test-model")
            .with_provider("test-provider");

        let mut step = fix.step(vec![]);

        plugin.run_phase(Phase::BeforeInference, &mut step).await;
        step.response = Some(StreamResult {
            text: "hi".into(),
            tool_calls: vec![],
            usage: Some(usage(10, 20, 30)),
        });
        plugin.run_phase(Phase::AfterInference, &mut step).await;

        let spans = captured.lock().unwrap();
        let inference_span = spans.iter().find(|s| s.name == "gen_ai");
        assert!(inference_span.is_some(), "expected gen_ai span (inference)");
        assert!(inference_span.unwrap().was_closed, "span should be closed");
    }

    #[tokio::test]
    async fn test_tracing_span_tool() {
        let fix = TestFixture::new();
        let captured = Arc::new(Mutex::new(Vec::<CapturedSpan>::new()));
        let layer = SpanCaptureLayer {
            captured: captured.clone(),
        };
        let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone());

        let mut step = fix.step(vec![]);

        let call = ToolCall::new("c1", "search", json!({}));
        step.tool = Some(PhaseToolContext::new(&call));

        plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
        step.tool.as_mut().unwrap().result =
            Some(ToolResult::success("search", json!({"found": true})));
        plugin.run_phase(Phase::AfterToolExecute, &mut step).await;

        let spans = captured.lock().unwrap();
        let tool_span = spans.iter().find(|s| s.name == "gen_ai");
        assert!(tool_span.is_some(), "expected gen_ai span (tool)");
        assert!(tool_span.unwrap().was_closed, "span should be closed");
    }

    #[test]
    fn test_plugin_id() {
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink);
        assert_eq!(AgentPlugin::id(&plugin), "llmmetry");
    }

    // ---- OTel export compatibility tests ----

    mod otel_export {
        use super::*;
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
        use tracing_opentelemetry::OpenTelemetryLayer;
        use tracing_subscriber::layer::SubscriberExt;

        fn setup_otel_test() -> (
            tracing::subscriber::DefaultGuard,
            InMemorySpanExporter,
            SdkTracerProvider,
        ) {
            let exporter = InMemorySpanExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_simple_exporter(exporter.clone())
                .build();
            let tracer = provider.tracer("test");
            let otel_layer = OpenTelemetryLayer::new(tracer);
            let subscriber = tracing_subscriber::registry::Registry::default().with(otel_layer);
            let guard = tracing::subscriber::set_default(subscriber);
            (guard, exporter, provider)
        }

        fn find_attribute<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
            span.attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| &kv.value)
        }

        #[tokio::test]
        async fn test_otel_export_inference_span() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink)
                .with_model("test-model")
                .with_provider("test-provider");

            let mut step = fix.step(vec![]);

            plugin.run_phase(Phase::BeforeInference, &mut step).await;
            step.response = Some(StreamResult {
                text: "hello".into(),
                tool_calls: vec![],
                usage: Some(usage(100, 50, 150)),
            });
            plugin.run_phase(Phase::AfterInference, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span in OTel export");

            assert_eq!(
                find_attribute(span, "gen_ai.provider.name")
                    .unwrap()
                    .as_str(),
                "test-provider"
            );
            assert_eq!(
                find_attribute(span, "gen_ai.operation.name")
                    .unwrap()
                    .as_str(),
                "chat"
            );
            assert_eq!(
                find_attribute(span, "gen_ai.request.model")
                    .unwrap()
                    .as_str(),
                "test-model"
            );
            assert_eq!(
                find_attribute(span, "gen_ai.usage.input_tokens"),
                Some(&opentelemetry::Value::I64(100))
            );
            assert_eq!(
                find_attribute(span, "gen_ai.usage.output_tokens"),
                Some(&opentelemetry::Value::I64(50))
            );
            // Duration is captured by OTel span timestamps, not as a custom attribute
            assert!(
                find_attribute(span, "gen_ai.client.operation.duration_ms").is_none(),
                "duration should not be a custom attribute (OTel captures it via span timestamps)"
            );
        }

        #[tokio::test]
        async fn test_otel_export_inference_error_sets_status_and_error_type() {
            let fix = TestFixture::new_with_state(json!({
                "__inference_error": {
                    "error": {
                        "type": "rate_limited",
                        "message": "429"
                    }
                }
            }));
            use opentelemetry::trace::Status;

            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink)
                .with_model("test-model")
                .with_provider("test-provider");

            let mut step = fix.step(vec![]);

            plugin.run_phase(Phase::BeforeInference, &mut step).await;
            plugin.run_phase(Phase::AfterInference, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span in OTel export");

            assert_eq!(
                find_attribute(span, "error.type").unwrap().as_str(),
                "rate_limited"
            );
            assert!(
                matches!(span.status, Status::Error { .. }),
                "failed inference span should have OTel Status::Error, got {:?}",
                span.status
            );
        }

        #[tokio::test]
        async fn test_otel_export_parent_child_propagation() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink)
                .with_model("test-model")
                .with_provider("test-provider");

            // Create and enter a parent span
            let parent = tracing::info_span!("parent_operation");
            let _parent_guard = parent.enter();

            let mut step = fix.step(vec![]);

            plugin.run_phase(Phase::BeforeInference, &mut step).await;

            step.response = Some(StreamResult {
                text: "hello".into(),
                tool_calls: vec![],
                usage: Some(usage(100, 50, 150)),
            });
            plugin.run_phase(Phase::AfterInference, &mut step).await;

            drop(_parent_guard);
            drop(parent);

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();

            let parent_span = exported
                .iter()
                .find(|s| s.name == "parent_operation")
                .expect("expected parent_operation span");
            let child_span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // The child span should share the same trace_id as the parent
            assert_eq!(
                child_span.span_context.trace_id(),
                parent_span.span_context.trace_id(),
                "chat span should share parent's trace_id"
            );
            // The child span's parent_span_id should be the parent's span_id
            assert_eq!(
                child_span.parent_span_id,
                parent_span.span_context.span_id(),
                "chat span should have parent's span_id as parent_span_id"
            );
        }

        #[tokio::test]
        async fn test_otel_export_tool_parent_child_propagation() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink);

            let parent = tracing::info_span!("parent_tool_op");
            let _parent_guard = parent.enter();

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "search", json!({}));
            step.tool = Some(PhaseToolContext::new(&call));

            plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

            step.tool.as_mut().unwrap().result =
                Some(ToolResult::success("search", json!({"found": true})));
            plugin.run_phase(Phase::AfterToolExecute, &mut step).await;

            drop(_parent_guard);
            drop(parent);

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();

            let parent_span = exported
                .iter()
                .find(|s| s.name == "parent_tool_op")
                .expect("expected parent_tool_op span");
            let child_span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            assert_eq!(
                child_span.span_context.trace_id(),
                parent_span.span_context.trace_id(),
                "execute_tool span should share parent's trace_id"
            );
            assert_eq!(
                child_span.parent_span_id,
                parent_span.span_context.span_id(),
                "execute_tool span should have parent's span_id as parent_span_id"
            );
        }

        #[tokio::test]
        async fn test_otel_export_no_parent_context() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            // No parent span entered — should be a root span
            let mut step = fix.step(vec![]);

            plugin.run_phase(Phase::BeforeInference, &mut step).await;
            step.response = Some(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: None,
            });
            plugin.run_phase(Phase::AfterInference, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();

            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // Root span should have invalid parent_span_id
            assert_eq!(
                span.parent_span_id,
                opentelemetry::trace::SpanId::INVALID,
                "root span should have no parent"
            );
        }

        #[tokio::test]
        async fn test_otel_export_spans_closed_after_phases() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            // Inference span should be closed (exported) after AfterInference
            {
                let mut step = fix.step(vec![]);
                plugin.run_phase(Phase::BeforeInference, &mut step).await;
                step.response = Some(StreamResult {
                    text: "hi".into(),
                    tool_calls: vec![],
                    usage: None,
                });
                plugin.run_phase(Phase::AfterInference, &mut step).await;
            }

            // Tool span should be closed (exported) after AfterToolExecute
            {
                let mut step = fix.step(vec![]);
                let call = ToolCall::new("c1", "test", json!({}));
                step.tool = Some(PhaseToolContext::new(&call));
                plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
                step.tool.as_mut().unwrap().result = Some(ToolResult::success("test", json!({})));
                plugin.run_phase(Phase::AfterToolExecute, &mut step).await;
            }

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            assert!(
                exported.iter().any(|s| s.name.starts_with("chat ")),
                "inference span should be exported (closed) after AfterInference"
            );
            assert!(
                exported.iter().any(|s| s.name.starts_with("execute_tool ")),
                "tool span should be exported (closed) after AfterToolExecute"
            );
        }

        #[tokio::test]
        async fn test_otel_export_inference_and_tool_are_siblings() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            let parent = tracing::info_span!("agent_step");
            let _parent_guard = parent.enter();

            // Inference phase
            {
                let mut step = fix.step(vec![]);
                plugin.run_phase(Phase::BeforeInference, &mut step).await;
                step.response = Some(StreamResult {
                    text: "calling tool".into(),
                    tool_calls: vec![ToolCall::new("c1", "search", json!({}))],
                    usage: Some(usage(10, 5, 15)),
                });
                plugin.run_phase(Phase::AfterInference, &mut step).await;
            }

            // Tool phase
            {
                let mut step = fix.step(vec![]);
                let call = ToolCall::new("c1", "search", json!({}));
                step.tool = Some(PhaseToolContext::new(&call));
                plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
                step.tool.as_mut().unwrap().result = Some(ToolResult::success("search", json!({})));
                plugin.run_phase(Phase::AfterToolExecute, &mut step).await;
            }

            drop(_parent_guard);
            drop(parent);

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();

            let parent_span = exported
                .iter()
                .find(|s| s.name == "agent_step")
                .expect("expected agent_step span");
            let inference_span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");
            let tool_span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            // Both should share the same trace
            assert_eq!(
                inference_span.span_context.trace_id(),
                parent_span.span_context.trace_id(),
            );
            assert_eq!(
                tool_span.span_context.trace_id(),
                parent_span.span_context.trace_id(),
            );

            // Both should be children of the parent (siblings, not nested)
            assert_eq!(
                inference_span.parent_span_id,
                parent_span.span_context.span_id(),
                "inference span should be child of agent_step"
            );
            assert_eq!(
                tool_span.parent_span_id,
                parent_span.span_context.span_id(),
                "tool span should be child of agent_step"
            );

            // They should be distinct spans
            assert_ne!(
                inference_span.span_context.span_id(),
                tool_span.span_context.span_id(),
                "inference and tool should be distinct sibling spans"
            );
        }

        #[tokio::test]
        async fn test_otel_export_tool_span() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink);

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "search", json!({}));
            step.tool = Some(PhaseToolContext::new(&call));

            plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
            step.tool.as_mut().unwrap().result =
                Some(ToolResult::success("search", json!({"found": true})));
            plugin.run_phase(Phase::AfterToolExecute, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span in OTel export");

            assert_eq!(
                find_attribute(span, "gen_ai.tool.name").unwrap().as_str(),
                "search"
            );
            assert_eq!(
                find_attribute(span, "gen_ai.tool.call.id")
                    .unwrap()
                    .as_str(),
                "tc1"
            );
            assert_eq!(
                find_attribute(span, "gen_ai.operation.name")
                    .unwrap()
                    .as_str(),
                "execute_tool"
            );
        }

        // ==================================================================
        // OTel GenAI Semantic Conventions compliance tests
        // ==================================================================

        #[tokio::test]
        async fn test_otel_semconv_inference_span_name_format() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink)
                .with_model("gpt-4o")
                .with_provider("openai");

            let mut step = fix.step(vec![]);

            plugin.run_phase(Phase::BeforeInference, &mut step).await;
            step.response = Some(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: None,
            });
            plugin.run_phase(Phase::AfterInference, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // Spec: span name = "{gen_ai.operation.name} {gen_ai.request.model}"
            assert_eq!(
                span.name.as_ref(),
                "chat gpt-4o",
                "span name should follow OTel GenAI format: '{{operation}} {{model}}'"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_tool_span_name_format() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_provider("openai");

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "web_search", json!({}));
            step.tool = Some(PhaseToolContext::new(&call));

            plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
            step.tool.as_mut().unwrap().result = Some(ToolResult::success("web_search", json!({})));
            plugin.run_phase(Phase::AfterToolExecute, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            // Spec: span name = "execute_tool {gen_ai.tool.name}"
            assert_eq!(
                span.name.as_ref(),
                "execute_tool web_search",
                "span name should follow OTel GenAI format: 'execute_tool {{tool_name}}'"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_inference_span_kind_client() {
            let fix = TestFixture::new();
            use opentelemetry::trace::SpanKind;

            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            let mut step = fix.step(vec![]);

            plugin.run_phase(Phase::BeforeInference, &mut step).await;
            step.response = Some(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: None,
            });
            plugin.run_phase(Phase::AfterInference, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // Spec: chat spans should be SpanKind::Client
            assert_eq!(
                span.span_kind,
                SpanKind::Client,
                "inference span should have SpanKind::Client per OTel GenAI spec"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_tool_span_kind_internal() {
            let fix = TestFixture::new();
            use opentelemetry::trace::SpanKind;

            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_provider("p");

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "search", json!({}));
            step.tool = Some(PhaseToolContext::new(&call));

            plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
            step.tool.as_mut().unwrap().result = Some(ToolResult::success("search", json!({})));
            plugin.run_phase(Phase::AfterToolExecute, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            // Spec: tool spans should be SpanKind::Internal
            assert_eq!(
                span.span_kind,
                SpanKind::Internal,
                "tool span should have SpanKind::Internal per OTel GenAI spec"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_tool_span_has_provider() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_provider("anthropic");

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "search", json!({}));
            step.tool = Some(PhaseToolContext::new(&call));

            plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
            step.tool.as_mut().unwrap().result = Some(ToolResult::success("search", json!({})));
            plugin.run_phase(Phase::AfterToolExecute, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            // Spec: gen_ai.provider.name is required on all GenAI spans
            assert_eq!(
                find_attribute(span, "gen_ai.provider.name")
                    .unwrap()
                    .as_str(),
                "anthropic",
                "tool span must include gen_ai.provider.name per OTel GenAI spec"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_error_sets_status_code() {
            let fix = TestFixture::new();
            use opentelemetry::trace::Status;

            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_provider("p");

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "write", json!({}));
            step.tool = Some(PhaseToolContext::new(&call));

            plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
            step.tool.as_mut().unwrap().result =
                Some(ToolResult::error("write", "permission denied"));
            plugin.run_phase(Phase::AfterToolExecute, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            // Spec: error.type should be set
            assert_eq!(
                find_attribute(span, "error.type").unwrap().as_str(),
                "tool_error"
            );

            // Spec: OTel status should be Error on failure
            assert!(
                matches!(span.status, Status::Error { .. }),
                "failed tool span should have OTel Status::Error, got {:?}",
                span.status
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_success_no_error_status() {
            let fix = TestFixture::new();
            use opentelemetry::trace::Status;

            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            let mut step = fix.step(vec![]);

            plugin.run_phase(Phase::BeforeInference, &mut step).await;
            step.response = Some(StreamResult {
                text: "ok".into(),
                tool_calls: vec![],
                usage: Some(usage(10, 5, 15)),
            });
            plugin.run_phase(Phase::AfterInference, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // Spec: successful spans should NOT have Error status
            assert!(
                !matches!(span.status, Status::Error { .. }),
                "successful span should not have Error status"
            );
            assert!(
                find_attribute(span, "error.type").is_none(),
                "successful span should not have error.type attribute"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_required_attributes_present() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink)
                .with_model("claude-3")
                .with_provider("anthropic");

            let mut step = fix.step(vec![]);

            plugin.run_phase(Phase::BeforeInference, &mut step).await;
            step.response = Some(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: Some(usage(100, 50, 150)),
            });
            plugin.run_phase(Phase::AfterInference, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // Required: gen_ai.operation.name
            assert_eq!(
                find_attribute(span, "gen_ai.operation.name")
                    .unwrap()
                    .as_str(),
                "chat"
            );
            // Required: gen_ai.provider.name
            assert_eq!(
                find_attribute(span, "gen_ai.provider.name")
                    .unwrap()
                    .as_str(),
                "anthropic"
            );
            // Conditionally required: gen_ai.request.model
            assert_eq!(
                find_attribute(span, "gen_ai.request.model")
                    .unwrap()
                    .as_str(),
                "claude-3"
            );
            // Recommended: gen_ai.usage.input_tokens
            assert_eq!(
                find_attribute(span, "gen_ai.usage.input_tokens"),
                Some(&opentelemetry::Value::I64(100))
            );
            // Recommended: gen_ai.usage.output_tokens
            assert_eq!(
                find_attribute(span, "gen_ai.usage.output_tokens"),
                Some(&opentelemetry::Value::I64(50))
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_no_usage_omits_token_attributes() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            let mut step = fix.step(vec![]);

            plugin.run_phase(Phase::BeforeInference, &mut step).await;
            step.response = Some(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: None,
            });
            plugin.run_phase(Phase::AfterInference, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // When usage is None, token attributes should not be recorded
            // (Empty fields are not exported as OTel attributes)
            assert!(
                find_attribute(span, "gen_ai.usage.input_tokens").is_none(),
                "input_tokens should be absent when usage is unavailable"
            );
            assert!(
                find_attribute(span, "gen_ai.usage.output_tokens").is_none(),
                "output_tokens should be absent when usage is unavailable"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_cache_tokens_only_when_present() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            // Test with cache tokens present
            {
                let mut step = fix.step(vec![]);
                plugin.run_phase(Phase::BeforeInference, &mut step).await;
                step.response = Some(StreamResult {
                    text: "hi".into(),
                    tool_calls: vec![],
                    usage: Some(usage_with_cache(100, 50, 150, 30)),
                });
                plugin.run_phase(Phase::AfterInference, &mut step).await;
            }

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            assert_eq!(
                find_attribute(span, "gen_ai.usage.cache_read.input_tokens"),
                Some(&opentelemetry::Value::I64(30)),
                "cache_read tokens should be recorded when present"
            );
            // cache_creation not set in test fixture
            assert!(
                find_attribute(span, "gen_ai.usage.cache_creation.input_tokens").is_none(),
                "cache_creation tokens should be absent when not provided"
            );
        }

        // ==================================================================
        // Request parameter + tool type tests
        // ==================================================================

        #[tokio::test]
        async fn test_plugin_captures_request_params() {
            let fix = TestFixture::new();
            let sink = InMemorySink::new();

            let opts = ChatOptions::default()
                .with_temperature(0.7)
                .with_max_tokens(2048)
                .with_top_p(0.9);

            let plugin = LLMMetryPlugin::new(sink.clone())
                .with_model("m")
                .with_provider("p")
                .with_chat_options(&opts);

            let mut step = fix.step(vec![]);

            plugin.run_phase(Phase::BeforeInference, &mut step).await;
            step.response = Some(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: Some(usage(10, 5, 15)),
            });
            plugin.run_phase(Phase::AfterInference, &mut step).await;

            let m = sink.metrics();
            let span = &m.inferences[0];
            assert_eq!(span.temperature, Some(0.7));
            assert_eq!(span.top_p, Some(0.9));
            assert_eq!(span.max_tokens, Some(2048));
            assert!(span.stop_sequences.is_empty());
        }

        #[tokio::test]
        async fn test_otel_export_request_params() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let opts = ChatOptions::default()
                .with_temperature(0.5)
                .with_max_tokens(1024);

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink)
                .with_model("m")
                .with_provider("p")
                .with_chat_options(&opts);

            let mut step = fix.step(vec![]);

            plugin.run_phase(Phase::BeforeInference, &mut step).await;
            step.response = Some(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: None,
            });
            plugin.run_phase(Phase::AfterInference, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            assert_eq!(
                find_attribute(span, "gen_ai.request.temperature"),
                Some(&opentelemetry::Value::F64(0.5)),
            );
            assert_eq!(
                find_attribute(span, "gen_ai.request.max_tokens"),
                Some(&opentelemetry::Value::I64(1024)),
            );
            // top_p not set → should be absent
            assert!(
                find_attribute(span, "gen_ai.request.top_p").is_none(),
                "top_p should be absent when not configured"
            );
        }

        #[tokio::test]
        async fn test_otel_export_request_params_omitted_when_none() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");
            // No with_chat_options → all request params are None

            let mut step = fix.step(vec![]);

            plugin.run_phase(Phase::BeforeInference, &mut step).await;
            step.response = Some(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: None,
            });
            plugin.run_phase(Phase::AfterInference, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            assert!(find_attribute(span, "gen_ai.request.temperature").is_none());
            assert!(find_attribute(span, "gen_ai.request.top_p").is_none());
            assert!(find_attribute(span, "gen_ai.request.max_tokens").is_none());
            assert!(find_attribute(span, "gen_ai.request.stop_sequences").is_none());
        }

        #[tokio::test]
        async fn test_otel_export_tool_type_function() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_provider("p");

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "search", json!({}));
            step.tool = Some(PhaseToolContext::new(&call));

            plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
            step.tool.as_mut().unwrap().result = Some(ToolResult::success("search", json!({})));
            plugin.run_phase(Phase::AfterToolExecute, &mut step).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            assert_eq!(
                find_attribute(span, "gen_ai.tool.type").unwrap().as_str(),
                "function",
                "tool span must include gen_ai.tool.type = 'function' per OTel GenAI spec"
            );
        }
    }
}
