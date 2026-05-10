//! OpenTelemetry export backend for observability metrics.
//!
//! Implements [`MetricsSink`] by mapping [`GenAISpan`] and [`ToolSpan`] to
//! OpenTelemetry spans using GenAI semantic conventions.
//!
//! Feature-gated behind `otel`.

use std::collections::{HashMap, HashSet, VecDeque};

use awaken_runtime::extensions::background::current_background_task_id;
use parking_lot::Mutex;

use opentelemetry::trace::{SpanContext as OtelSpanContext, SpanId, SpanKind, Status, Tracer};
use opentelemetry::{Array, KeyValue, StringValue, Value, trace::TraceContextExt};
use opentelemetry_sdk::trace::SdkTracer;

use crate::metrics::{
    AgentMetrics, BackgroundTaskSpan, DelegationSpan, EvaluationResultEvent, GenAISpan,
    HandoffSpan, MetricsEvent, SpanContext, SuspensionSpan, ToolSpan,
};
use crate::otel_config::OtelConfig;
use crate::sink::MetricsSink;

const MAX_RETAINED_CONTEXTS: usize = 4096;
const DEFAULT_RUN_KEY: &str = "__awaken_default_run__";

/// OpenTelemetry-based metrics sink.
///
/// Records each inference and tool span as an OTel span using the
/// GenAI semantic conventions, arranged in a proper parent-child
/// hierarchy:
///
/// ```text
/// invoke_agent <agent> (root, SpanKind::Internal)
///   ├─ chat gpt-4 (inference, SpanKind::Client)
///   │    ├─ execute_tool search (SpanKind::Internal)
///   │    └─ execute_tool read   (SpanKind::Internal)
///   └─ chat gpt-4 (inference, SpanKind::Client)
///        └─ execute_tool write  (SpanKind::Internal)
/// ```
///
/// The root agent span is lazily created on first `record()` and
/// ended when `on_run_end()` is called.
pub struct OtelMetricsSink {
    tracer: SdkTracer,
    /// Root agent invocation span contexts keyed by run id.
    root_contexts: Mutex<HashMap<String, opentelemetry::Context>>,
    /// Current inference span context — tool spans and evaluation events become
    /// children/events of this span until the next inference or run end.
    current_inferences: Mutex<HashMap<String, ActiveInference>>,
    /// Tool span contexts retained briefly so async/background work can attach
    /// to the tool call that spawned it.
    tool_contexts: Mutex<ContextCache>,
    /// Tool contexts created before the matching ToolSpan arrives.
    pending_tool_spans: Mutex<HashMap<String, PendingToolSpan>>,
    /// Background task spans that may outlive the run that created them.
    current_background_tasks: Mutex<HashMap<String, ActiveBackgroundTask>>,
    /// Background task contexts retained for nested background task lineage.
    background_task_contexts: Mutex<ContextCache>,
    /// Root spans whose run ended but still have open background task children.
    deferred_root_ends: Mutex<HashMap<String, Vec<KeyValue>>>,
}

#[derive(Clone)]
struct ActiveInference {
    cx: opentelemetry::Context,
    end_time: std::time::SystemTime,
}

#[derive(Clone)]
struct PendingToolSpan {
    parent_cx: opentelemetry::Context,
    reserved_cx: opentelemetry::Context,
    span_id: SpanId,
    parent_run_id: String,
    call_id: String,
    /// Earliest observed child timestamp (epoch ms) used as the synthetic
    /// start when no real `ToolSpan` ever arrives. Synthesizing with `now`
    /// would place the parent later than its child in the trace timeline.
    earliest_child_ms: Option<u64>,
}

#[derive(Clone)]
struct ActiveBackgroundTask {
    cx: opentelemetry::Context,
    run_key: String,
}

#[derive(Default)]
struct ContextCache {
    contexts: HashMap<String, opentelemetry::Context>,
    order: VecDeque<String>,
}

impl ContextCache {
    fn insert(&mut self, key: String, cx: opentelemetry::Context) {
        if !self.contexts.contains_key(&key) {
            self.order.push_back(key.clone());
        }
        self.contexts.insert(key, cx);
        while self.contexts.len() > MAX_RETAINED_CONTEXTS {
            if let Some(oldest) = self.order.pop_front() {
                self.contexts.remove(&oldest);
            } else {
                break;
            }
        }
    }

    fn get(&self, key: &str) -> Option<opentelemetry::Context> {
        self.contexts.get(key).cloned()
    }

    fn remove(&mut self, key: &str) {
        self.contexts.remove(key);
        self.order.retain(|existing| existing != key);
    }
}

struct RootSpanSeed<'a> {
    context: &'a SpanContext,
    provider: Option<&'a str>,
    model: Option<&'a str>,
}

impl OtelMetricsSink {
    /// Create a new OTel sink with the given SDK tracer.
    pub fn new(tracer: SdkTracer) -> Self {
        Self {
            tracer,
            root_contexts: Mutex::new(HashMap::new()),
            current_inferences: Mutex::new(HashMap::new()),
            tool_contexts: Mutex::new(ContextCache::default()),
            pending_tool_spans: Mutex::new(HashMap::new()),
            current_background_tasks: Mutex::new(HashMap::new()),
            background_task_contexts: Mutex::new(ContextCache::default()),
            deferred_root_ends: Mutex::new(HashMap::new()),
        }
    }

    /// Return the root agent invocation context, creating it lazily.
    fn ensure_root_context(&self, seed: RootSpanSeed<'_>) -> opentelemetry::Context {
        let run_key = Self::run_key(seed.context);
        {
            let root_contexts = self.root_contexts.lock();
            if let Some(cx) = root_contexts.get(&run_key) {
                cx.span()
                    .set_attributes(Self::root_agent_update_attributes(&seed));
                return cx.clone();
            }
        }

        let span_name = if seed.context.agent_id.is_empty() {
            "invoke_agent".to_string()
        } else {
            format!("invoke_agent {}", seed.context.agent_id)
        };
        let builder = self
            .tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Internal)
            .with_attributes(Self::root_agent_attributes(&seed, true));
        let parent_cx = self.parent_context_for_root(seed.context);
        let root_span = if let Some(parent_cx) = parent_cx {
            builder.start_with_context(&self.tracer, &parent_cx)
        } else {
            builder.start(&self.tracer)
        };
        let cx = opentelemetry::Context::new().with_span(root_span);
        self.root_contexts.lock().insert(run_key, cx.clone());
        cx
    }

    fn run_key(ctx: &SpanContext) -> String {
        if ctx.run_id.is_empty() {
            DEFAULT_RUN_KEY.to_string()
        } else {
            ctx.run_id.clone()
        }
    }

    fn context_key(run_key: &str, id: &str) -> String {
        format!("{run_key}\u{1f}{id}")
    }

    fn tool_context_key(run_key: &str, call_id: &str) -> String {
        Self::context_key(run_key, call_id)
    }

    fn task_context_key(task_id: &str) -> String {
        task_id.to_string()
    }

    fn new_span_id() -> SpanId {
        let uuid = uuid::Uuid::now_v7();
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&uuid.as_bytes()[8..]);
        let span_id = SpanId::from_bytes(bytes);
        if span_id == SpanId::INVALID {
            SpanId::from_bytes([1, 0, 0, 0, 0, 0, 0, 0])
        } else {
            span_id
        }
    }

    fn ambient_parent_task_id() -> Option<String> {
        current_background_task_id().filter(|id| !id.is_empty())
    }

    fn parent_context_for_root(&self, ctx: &SpanContext) -> Option<opentelemetry::Context> {
        if let Some(parent_task_id) = Self::ambient_parent_task_id() {
            return Some(
                self.ensure_background_task_context_from_span_context(ctx, &parent_task_id),
            );
        }

        let parent_run_id = ctx.parent_run_id.as_deref().filter(|id| !id.is_empty())?;
        if let Some(parent_tool_call_id) = ctx
            .parent_tool_call_id
            .as_deref()
            .filter(|id| !id.is_empty())
        {
            let tool_key = Self::tool_context_key(parent_run_id, parent_tool_call_id);
            if let Some(cx) = self.tool_contexts.lock().get(&tool_key) {
                return Some(cx);
            }
        }
        if let Some(active) = self.current_inferences.lock().get(parent_run_id).cloned() {
            return Some(active.cx);
        }
        self.root_contexts.lock().get(parent_run_id).cloned()
    }

    fn parent_context_for_event(&self, ctx: &SpanContext) -> opentelemetry::Context {
        let run_key = Self::run_key(ctx);
        if let Some(parent_tool_call_id) = ctx
            .parent_tool_call_id
            .as_deref()
            .filter(|id| !id.is_empty())
        {
            let tool_key = Self::tool_context_key(&run_key, parent_tool_call_id);
            if let Some(cx) = self.tool_contexts.lock().get(&tool_key) {
                return cx;
            }
        }
        if let Some(active) = self.current_inferences.lock().get(&run_key).cloned() {
            active.cx
        } else {
            self.ensure_root_context(RootSpanSeed {
                context: ctx,
                provider: None,
                model: None,
            })
        }
    }

    fn root_agent_update_attributes(seed: &RootSpanSeed<'_>) -> Vec<KeyValue> {
        Self::root_agent_attributes(seed, false)
    }

    fn root_agent_attributes(seed: &RootSpanSeed<'_>, fallback_provider: bool) -> Vec<KeyValue> {
        let mut attrs = vec![KeyValue::new("gen_ai.operation.name", "invoke_agent")];
        if let Some(provider) = seed.provider.filter(|v| !v.is_empty()) {
            attrs.push(KeyValue::new("gen_ai.provider.name", provider.to_string()));
        } else if fallback_provider {
            attrs.push(KeyValue::new("gen_ai.provider.name", "awaken"));
        }
        if let Some(model) = seed.model.filter(|v| !v.is_empty()) {
            attrs.push(KeyValue::new("gen_ai.request.model", model.to_string()));
        }
        Self::push_genai_context_attributes(&mut attrs, seed.context);
        Self::push_awaken_context_attributes(&mut attrs, seed.context);
        attrs
    }

    /// Append Awaken-specific execution context attributes.
    fn push_awaken_context_attributes(attrs: &mut Vec<KeyValue>, ctx: &SpanContext) {
        if !ctx.run_id.is_empty() {
            attrs.push(KeyValue::new("awaken.run.id", ctx.run_id.clone()));
        }
        if !ctx.thread_id.is_empty() {
            attrs.push(KeyValue::new("awaken.thread.id", ctx.thread_id.clone()));
        }
        if !ctx.agent_id.is_empty() {
            attrs.push(KeyValue::new("awaken.agent.id", ctx.agent_id.clone()));
        }
        if let Some(ref parent) = ctx.parent_run_id {
            attrs.push(KeyValue::new("awaken.parent_run.id", parent.clone()));
        }
        if let Some(ref call_id) = ctx.parent_tool_call_id {
            attrs.push(KeyValue::new("awaken.parent_tool.call_id", call_id.clone()));
        }
        if let Some(task_id) = Self::ambient_parent_task_id() {
            attrs.push(KeyValue::new("awaken.parent_task.id", task_id.clone()));
        }
    }

    /// Append standard GenAI correlation attributes when available.
    fn push_genai_context_attributes(attrs: &mut Vec<KeyValue>, ctx: &SpanContext) {
        if !ctx.thread_id.is_empty() {
            attrs.push(KeyValue::new(
                "gen_ai.conversation.id",
                ctx.thread_id.clone(),
            ));
        }
        if !ctx.agent_id.is_empty() {
            attrs.push(KeyValue::new("gen_ai.agent.id", ctx.agent_id.clone()));
        }
    }

    fn string_array(values: &[String]) -> Value {
        Value::Array(Array::String(
            values.iter().cloned().map(StringValue::from).collect(),
        ))
    }

    fn json_value_attr(value: &serde_json::Value) -> Option<String> {
        serde_json::to_string(value).ok()
    }

    /// Build OTel attributes from a GenAI inference span.
    fn genai_attributes(span: &GenAISpan) -> Vec<KeyValue> {
        let mut attrs = vec![
            KeyValue::new("gen_ai.provider.name", span.provider.clone()),
            KeyValue::new("gen_ai.request.model", span.model.clone()),
            KeyValue::new("gen_ai.operation.name", span.operation.clone()),
        ];

        Self::push_genai_context_attributes(&mut attrs, &span.context);
        Self::push_awaken_context_attributes(&mut attrs, &span.context);
        if let Some(step) = span.step_index {
            attrs.push(KeyValue::new("awaken.step.index", step as i64));
        }

        if let Some(ref response_model) = span.response_model {
            attrs.push(KeyValue::new(
                "gen_ai.response.model",
                response_model.clone(),
            ));
        }
        if let Some(ref response_id) = span.response_id {
            attrs.push(KeyValue::new("gen_ai.response.id", response_id.clone()));
        }
        if !span.finish_reasons.is_empty() {
            attrs.push(KeyValue::new(
                "gen_ai.response.finish_reasons",
                Self::string_array(&span.finish_reasons),
            ));
        }

        // Token usage
        if let Some(input) = span.input_tokens {
            attrs.push(KeyValue::new("gen_ai.usage.input_tokens", i64::from(input)));
        }
        if let Some(output) = span.output_tokens {
            attrs.push(KeyValue::new(
                "gen_ai.usage.output_tokens",
                i64::from(output),
            ));
        }
        if let Some(cache_read) = span.cache_read_input_tokens {
            attrs.push(KeyValue::new(
                "gen_ai.usage.cache_read.input_tokens",
                i64::from(cache_read),
            ));
        }
        if let Some(cache_creation) = span.cache_creation_input_tokens {
            attrs.push(KeyValue::new(
                "gen_ai.usage.cache_creation.input_tokens",
                i64::from(cache_creation),
            ));
        }
        if let Some(thinking) = span.thinking_tokens {
            attrs.push(KeyValue::new(
                "gen_ai.usage.reasoning.output_tokens",
                i64::from(thinking),
            ));
        }

        // Request parameters
        if let Some(temp) = span.temperature {
            attrs.push(KeyValue::new("gen_ai.request.temperature", temp));
        }
        if let Some(top_p) = span.top_p {
            attrs.push(KeyValue::new("gen_ai.request.top_p", top_p));
        }
        if let Some(max_tokens) = span.max_tokens {
            attrs.push(KeyValue::new(
                "gen_ai.request.max_tokens",
                i64::from(max_tokens),
            ));
        }
        if !span.stop_sequences.is_empty() {
            attrs.push(KeyValue::new(
                "gen_ai.request.stop_sequences",
                Self::string_array(&span.stop_sequences),
            ));
        }

        // Error
        if let Some(ref error_type) = span.error_type {
            attrs.push(KeyValue::new("error.type", error_type.clone()));
        }

        attrs
    }

    /// Build OTel attributes from a tool execution span.
    fn tool_attributes(span: &ToolSpan) -> Vec<KeyValue> {
        let mut attrs = vec![
            KeyValue::new("gen_ai.tool.name", span.name.clone()),
            KeyValue::new("gen_ai.operation.name", span.operation.clone()),
            KeyValue::new("gen_ai.tool.call.id", span.call_id.clone()),
            KeyValue::new("gen_ai.tool.type", span.tool_type.clone()),
        ];

        Self::push_awaken_context_attributes(&mut attrs, &span.context);
        if let Some(step) = span.step_index {
            attrs.push(KeyValue::new("awaken.step.index", step as i64));
        }
        if let Some(arguments) = &span.call_arguments
            && let Some(serialized) = Self::json_value_attr(arguments)
        {
            attrs.push(KeyValue::new("gen_ai.tool.call.arguments", serialized));
        }
        if let Some(result) = &span.call_result
            && let Some(serialized) = Self::json_value_attr(result)
        {
            attrs.push(KeyValue::new("gen_ai.tool.call.result", serialized));
        }
        if span.has_truncated_payload() {
            attrs.push(KeyValue::new("awaken.tool.payload.truncated", true));
        }

        if let Some(ref error_type) = span.error_type {
            attrs.push(KeyValue::new("error.type", error_type.clone()));
        }

        attrs
    }

    fn record_inference(&self, span: &GenAISpan) {
        let run_key = Self::run_key(&span.context);
        self.end_current_inference(&run_key);

        let attrs = Self::genai_attributes(span);
        let span_name = format!("{} {}", span.operation, span.model);

        let (start_time, end_time) =
            Self::span_window(span.started_at_ms, span.ended_at_ms, span.duration_ms);

        let root_cx = self.ensure_root_context(RootSpanSeed {
            context: &span.context,
            provider: Some(span.provider.as_str()),
            model: Some(span.model.as_str()),
        });

        let otel_span = self
            .tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Client)
            .with_attributes(attrs)
            .with_start_time(start_time)
            .start_with_context(&self.tracer, &root_cx);

        let inference_cx = root_cx.with_span(otel_span);

        if span.error_type.is_some() {
            inference_cx
                .span()
                .set_status(Status::error(span.error_type.clone().unwrap_or_default()));
        }

        // Store open span so tool spans become children and evaluation events
        // can be attached before the inference is ended with the recorded
        // model-call end timestamp.
        self.current_inferences.lock().insert(
            run_key,
            ActiveInference {
                cx: inference_cx,
                end_time,
            },
        );
    }

    fn end_current_inference(&self, run_key: &str) {
        if let Some(active) = self.current_inferences.lock().remove(run_key) {
            active.cx.span().end_with_timestamp(active.end_time);
        }
    }

    fn end_all_current_inferences(&self) {
        for (_, active) in self.current_inferences.lock().drain() {
            active.cx.span().end_with_timestamp(active.end_time);
        }
    }

    fn end_pending_tool_spans_for_run(&self, run_key: &str) {
        let prefix = format!("{run_key}\u{1f}");
        let keys = {
            self.pending_tool_spans
                .lock()
                .keys()
                .filter(|key| key.starts_with(&prefix))
                .cloned()
                .collect::<Vec<_>>()
        };
        for key in keys {
            if let Some(pending) = self.pending_tool_spans.lock().remove(&key) {
                self.tool_contexts.lock().remove(&key);
                self.end_synthetic_tool_span(pending);
            }
        }
    }

    fn end_all_pending_tool_spans(&self) {
        let pending = self.pending_tool_spans.lock().drain().collect::<Vec<_>>();
        for (key, pending) in pending {
            self.tool_contexts.lock().remove(&key);
            self.end_synthetic_tool_span(pending);
        }
    }

    fn end_synthetic_tool_span(&self, pending: PendingToolSpan) {
        let end = std::time::SystemTime::now();
        // Anchor the synthetic span at the earliest observed child so the
        // parent never appears later than its child in the trace timeline.
        let start = pending
            .earliest_child_ms
            .map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms))
            .unwrap_or(end);
        let mut attrs = Self::lazy_tool_attributes(&pending.parent_run_id, &pending.call_id);
        attrs.push(KeyValue::new("awaken.tool.synthetic_parent", true));
        let span = self
            .tracer
            .span_builder("execute_tool")
            .with_kind(SpanKind::Internal)
            .with_span_id(pending.span_id)
            .with_attributes(attrs)
            .with_start_time(start)
            .start_with_context(&self.tracer, &pending.parent_cx);
        pending
            .parent_cx
            .with_span(span)
            .span()
            .end_with_timestamp(end);
    }

    fn record_tool(&self, span: &ToolSpan) {
        let attrs = Self::tool_attributes(span);
        let span_name = format!("execute_tool {}", span.name);

        let (start_time, end_time) =
            Self::span_window(span.started_at_ms, span.ended_at_ms, span.duration_ms);
        let tool_key = if span.call_id.is_empty() {
            None
        } else {
            Some(Self::tool_context_key(
                &Self::run_key(&span.context),
                &span.call_id,
            ))
        };

        if let Some(key) = tool_key.as_deref()
            && let Some(pending) = self.pending_tool_spans.lock().remove(key)
        {
            let otel_span = self
                .tracer
                .span_builder(span_name)
                .with_kind(SpanKind::Internal)
                .with_span_id(pending.span_id)
                .with_attributes(attrs)
                .with_start_time(start_time)
                .start_with_context(&self.tracer, &pending.parent_cx);
            let cx = pending.parent_cx.with_span(otel_span);
            if span.error_type.is_some() {
                cx.span()
                    .set_status(Status::error(span.error_type.clone().unwrap_or_default()));
            }
            cx.span().end_with_timestamp(end_time);
            self.tool_contexts.lock().insert(key.to_string(), cx);
            return;
        }

        // Prefer this run's current inference as parent; fall back to root.
        let parent_cx = self.parent_context_for_event(&span.context);

        let otel_span = self
            .tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Internal)
            .with_attributes(attrs)
            .with_start_time(start_time)
            .start_with_context(&self.tracer, &parent_cx);

        let cx = parent_cx.with_span(otel_span);

        if span.error_type.is_some() {
            cx.span()
                .set_status(Status::error(span.error_type.clone().unwrap_or_default()));
        }

        if let Some(key) = tool_key {
            self.tool_contexts.lock().insert(key, cx.clone());
        }

        cx.span().end_with_timestamp(end_time);
    }

    fn record_suspension(&self, span: &SuspensionSpan) {
        let mut attrs = vec![
            KeyValue::new("awaken.suspension.action", span.action.clone()),
            KeyValue::new("gen_ai.tool.call.id", span.tool_call_id.clone()),
            KeyValue::new("gen_ai.tool.name", span.tool_name.clone()),
        ];
        Self::push_awaken_context_attributes(&mut attrs, &span.context);
        if let Some(resume_mode) = &span.resume_mode {
            attrs.push(KeyValue::new(
                "awaken.suspension.resume_mode",
                resume_mode.clone(),
            ));
        }
        if let Some(duration_ms) = span.duration_ms {
            attrs.push(KeyValue::new(
                "awaken.suspension.duration",
                duration_ms as f64 / 1000.0,
            ));
        }
        self.record_internal_span("awaken.suspension", &span.context, attrs);
    }

    fn record_handoff(&self, span: &HandoffSpan) {
        let mut attrs = vec![
            KeyValue::new("awaken.handoff.from_agent_id", span.from_agent_id.clone()),
            KeyValue::new("awaken.handoff.to_agent_id", span.to_agent_id.clone()),
        ];
        Self::push_awaken_context_attributes(&mut attrs, &span.context);
        if let Some(reason) = &span.reason {
            attrs.push(KeyValue::new("awaken.handoff.reason", reason.clone()));
        }
        self.record_internal_span("awaken.handoff", &span.context, attrs);
    }

    fn record_delegation(&self, span: &DelegationSpan) {
        let mut attrs = vec![
            KeyValue::new(
                "awaken.delegation.parent_run_id",
                span.parent_run_id.clone(),
            ),
            KeyValue::new(
                "awaken.delegation.target_agent_id",
                span.target_agent_id.clone(),
            ),
            KeyValue::new("gen_ai.tool.call.id", span.tool_call_id.clone()),
            KeyValue::new("awaken.delegation.success", span.success),
        ];
        Self::push_awaken_context_attributes(&mut attrs, &span.context);
        if let Some(child_run_id) = &span.child_run_id {
            attrs.push(KeyValue::new(
                "awaken.delegation.child_run_id",
                child_run_id.clone(),
            ));
        }
        if let Some(duration_ms) = span.duration_ms {
            attrs.push(KeyValue::new(
                "awaken.delegation.duration",
                duration_ms as f64 / 1000.0,
            ));
        }
        if let Some(error_message) = &span.error_message {
            attrs.push(KeyValue::new("error.message", error_message.clone()));
        }
        self.record_internal_span("awaken.delegation", &span.context, attrs);
    }

    fn background_task_attributes(span: &BackgroundTaskSpan) -> Vec<KeyValue> {
        let mut attrs = vec![
            KeyValue::new("awaken.operation.name", "background_task"),
            KeyValue::new("awaken.background_task.id", span.task_id.clone()),
            KeyValue::new("awaken.background_task.type", span.task_type.clone()),
            KeyValue::new("awaken.background_task.status", span.status.as_str()),
            KeyValue::new(
                "awaken.background_task.description",
                span.description.clone(),
            ),
        ];
        Self::push_awaken_context_attributes(&mut attrs, &span.context);
        if !span.context.run_id.is_empty() {
            attrs.push(KeyValue::new(
                "awaken.background_task.parent_run_id",
                span.context.run_id.clone(),
            ));
        }
        if let Some(task_name) = &span.task_name {
            attrs.push(KeyValue::new(
                "awaken.background_task.name",
                task_name.clone(),
            ));
        }
        if let Some(parent_task_id) = &span.parent_task_id {
            attrs.push(KeyValue::new(
                "awaken.parent_task.id",
                parent_task_id.clone(),
            ));
        }
        if let Some(parent_tool_call_id) = &span.context.parent_tool_call_id {
            attrs.push(KeyValue::new(
                "awaken.background_task.parent_tool_call_id",
                parent_tool_call_id.clone(),
            ));
        }
        if let Some(error_message) = &span.error_message {
            attrs.push(KeyValue::new("error.type", "background_task_error"));
            attrs.push(KeyValue::new("error.message", error_message.clone()));
        }
        attrs
    }

    fn background_task_context(&self, task_id: &str) -> Option<opentelemetry::Context> {
        if let Some(active) = self.current_background_tasks.lock().get(task_id).cloned() {
            return Some(active.cx);
        }
        self.background_task_contexts
            .lock()
            .get(&Self::task_context_key(task_id))
    }

    fn run_key_for_background_context(ctx: &SpanContext) -> String {
        ctx.parent_run_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| Self::run_key(ctx))
    }

    fn parent_run_context(&self, run_key: &str) -> Option<opentelemetry::Context> {
        if let Some(active) = self.current_inferences.lock().get(run_key).cloned() {
            return Some(active.cx);
        }
        self.root_contexts.lock().get(run_key).cloned()
    }

    fn lazy_tool_attributes(parent_run_id: &str, call_id: &str) -> Vec<KeyValue> {
        vec![
            KeyValue::new("gen_ai.operation.name", "execute_tool"),
            KeyValue::new("gen_ai.tool.call.id", call_id.to_string()),
            KeyValue::new("awaken.run.id", parent_run_id.to_string()),
        ]
    }

    fn ensure_lazy_tool_context(
        &self,
        parent_run_id: &str,
        parent_tool_call_id: &str,
    ) -> Option<opentelemetry::Context> {
        let key = Self::tool_context_key(parent_run_id, parent_tool_call_id);
        if let Some(cx) = self.pending_tool_spans.lock().get(&key).cloned() {
            return Some(cx.reserved_cx);
        }
        if let Some(cx) = self.tool_contexts.lock().get(&key) {
            return Some(cx);
        }

        let parent_cx = self.parent_run_context(parent_run_id)?;
        let parent_span_context = parent_cx.span().span_context().clone();
        let span_id = Self::new_span_id();
        let span_context = OtelSpanContext::new(
            parent_span_context.trace_id(),
            span_id,
            parent_span_context.trace_flags(),
            false,
            parent_span_context.trace_state().clone(),
        );
        let cx = parent_cx.with_remote_span_context(span_context);

        self.tool_contexts.lock().insert(key.clone(), cx.clone());
        self.pending_tool_spans.lock().insert(
            key,
            PendingToolSpan {
                parent_cx,
                reserved_cx: cx.clone(),
                span_id,
                parent_run_id: parent_run_id.to_string(),
                call_id: parent_tool_call_id.to_string(),
                earliest_child_ms: None,
            },
        );
        Some(cx)
    }

    fn parent_context_for_background_lineage(
        &self,
        ctx: &SpanContext,
    ) -> Option<opentelemetry::Context> {
        let parent_run_id = ctx.parent_run_id.as_deref().filter(|id| !id.is_empty())?;
        if let Some(parent_tool_call_id) = ctx
            .parent_tool_call_id
            .as_deref()
            .filter(|id| !id.is_empty())
        {
            let key = Self::tool_context_key(parent_run_id, parent_tool_call_id);
            if let Some(cx) = self.tool_contexts.lock().get(&key) {
                return Some(cx);
            }
            return self.ensure_lazy_tool_context(parent_run_id, parent_tool_call_id);
        }
        self.parent_run_context(parent_run_id)
    }

    fn lazy_background_task_attributes(ctx: &SpanContext, task_id: &str) -> Vec<KeyValue> {
        let mut attrs = vec![
            KeyValue::new("awaken.operation.name", "background_task"),
            KeyValue::new("awaken.background_task.id", task_id.to_string()),
            KeyValue::new("awaken.background_task.status", "running"),
        ];
        if let Some(parent_run_id) = ctx.parent_run_id.as_deref().filter(|id| !id.is_empty()) {
            attrs.push(KeyValue::new(
                "awaken.background_task.parent_run_id",
                parent_run_id.to_string(),
            ));
            attrs.push(KeyValue::new("awaken.run.id", parent_run_id.to_string()));
        }
        if !ctx.thread_id.is_empty() {
            attrs.push(KeyValue::new("awaken.thread.id", ctx.thread_id.clone()));
        }
        if let Some(parent_tool_call_id) = &ctx.parent_tool_call_id {
            attrs.push(KeyValue::new(
                "awaken.background_task.parent_tool_call_id",
                parent_tool_call_id.clone(),
            ));
            attrs.push(KeyValue::new(
                "awaken.parent_tool.call_id",
                parent_tool_call_id.clone(),
            ));
        }
        attrs
    }

    fn ensure_background_task_context_from_span_context(
        &self,
        ctx: &SpanContext,
        task_id: &str,
    ) -> opentelemetry::Context {
        if let Some(cx) = self.background_task_context(task_id) {
            return cx;
        }

        let parent_cx = self
            .parent_context_for_background_lineage(ctx)
            .unwrap_or_default();
        let otel_span = self
            .tracer
            .span_builder("awaken.background_task")
            .with_kind(SpanKind::Internal)
            .with_attributes(Self::lazy_background_task_attributes(ctx, task_id))
            .start_with_context(&self.tracer, &parent_cx);
        let cx = parent_cx.with_span(otel_span);

        self.background_task_contexts
            .lock()
            .insert(Self::task_context_key(task_id), cx.clone());
        self.current_background_tasks.lock().insert(
            task_id.to_string(),
            ActiveBackgroundTask {
                cx: cx.clone(),
                run_key: Self::run_key_for_background_context(ctx),
            },
        );
        cx
    }

    fn parent_context_for_background_task(
        &self,
        span: &BackgroundTaskSpan,
    ) -> opentelemetry::Context {
        if let Some(parent_task_id) = span.parent_task_id.as_deref().filter(|id| !id.is_empty())
            && let Some(cx) = self.background_task_context(parent_task_id)
        {
            return cx;
        }
        if let Some(parent_tool_call_id) = span
            .context
            .parent_tool_call_id
            .as_deref()
            .filter(|id| !id.is_empty())
        {
            let run_key = Self::run_key(&span.context);
            let key = Self::tool_context_key(&run_key, parent_tool_call_id);
            if let Some(cx) = self.tool_contexts.lock().get(&key) {
                return cx;
            }
            if let Some(cx) = self.ensure_lazy_tool_context(&run_key, parent_tool_call_id) {
                // Stamp the earliest child time so a synthetic parent can be
                // anchored at the right point on the timeline.
                if let Some(pending) = self.pending_tool_spans.lock().get_mut(&key) {
                    pending.earliest_child_ms = Some(match pending.earliest_child_ms {
                        Some(prev) => prev.min(span.created_at_ms),
                        None => span.created_at_ms,
                    });
                }
                return cx;
            }
        }
        self.parent_context_for_event(&span.context)
    }

    fn record_background_task(&self, span: &BackgroundTaskSpan) {
        let attrs = Self::background_task_attributes(span);
        let start_time =
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(span.created_at_ms);
        let end_time = span
            .completed_at_ms
            .map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms))
            .unwrap_or_else(std::time::SystemTime::now);

        let active = {
            self.current_background_tasks
                .lock()
                .get(&span.task_id)
                .cloned()
        };
        if let Some(active) = active {
            active.cx.span().set_attributes(attrs);
            if span.error_message.is_some() {
                active.cx.span().set_status(Status::error(
                    span.error_message.clone().unwrap_or_default(),
                ));
            }
            if span.is_terminal() {
                self.current_background_tasks.lock().remove(&span.task_id);
                active.cx.span().end_with_timestamp(end_time);
                self.end_deferred_root_if_background_idle(&active.run_key);
            }
            return;
        }

        let parent_cx = self.parent_context_for_background_task(span);
        let otel_span = self
            .tracer
            .span_builder("awaken.background_task")
            .with_kind(SpanKind::Internal)
            .with_attributes(attrs)
            .with_start_time(start_time)
            .start_with_context(&self.tracer, &parent_cx);
        let cx = parent_cx.with_span(otel_span);

        if span.error_message.is_some() {
            cx.span().set_status(Status::error(
                span.error_message.clone().unwrap_or_default(),
            ));
        }

        self.background_task_contexts
            .lock()
            .insert(Self::task_context_key(&span.task_id), cx.clone());

        if span.is_terminal() {
            cx.span().end_with_timestamp(end_time);
            self.end_deferred_root_if_background_idle(&Self::run_key_for_background_context(
                &span.context,
            ));
        } else {
            self.current_background_tasks.lock().insert(
                span.task_id.clone(),
                ActiveBackgroundTask {
                    cx,
                    run_key: Self::run_key_for_background_context(&span.context),
                },
            );
        }
    }

    fn record_evaluation_result(&self, event: &EvaluationResultEvent) {
        let mut attrs = vec![KeyValue::new("gen_ai.evaluation.name", event.name.clone())];
        if let Some(label) = &event.score_label {
            attrs.push(KeyValue::new(
                "gen_ai.evaluation.score.label",
                label.clone(),
            ));
        }
        if let Some(value) = event.score_value {
            attrs.push(KeyValue::new("gen_ai.evaluation.score.value", value));
        }
        if let Some(explanation) = &event.explanation {
            attrs.push(KeyValue::new(
                "gen_ai.evaluation.explanation",
                explanation.clone(),
            ));
        }
        if let Some(response_id) = &event.response_id {
            attrs.push(KeyValue::new("gen_ai.response.id", response_id.clone()));
        }
        if let Some(error_type) = &event.error_type {
            attrs.push(KeyValue::new("error.type", error_type.clone()));
        }
        Self::push_awaken_context_attributes(&mut attrs, &event.context);

        let parent_cx = self.parent_context_for_event(&event.context);
        parent_cx.span().add_event_with_timestamp(
            "gen_ai.evaluation.result",
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(event.timestamp_ms),
            attrs,
        );
    }

    fn record_internal_span(&self, name: &'static str, ctx: &SpanContext, attrs: Vec<KeyValue>) {
        let parent_cx = self.parent_context_for_event(ctx);
        let span = self
            .tracer
            .span_builder(name)
            .with_kind(SpanKind::Internal)
            .with_attributes(attrs)
            .start_with_context(&self.tracer, &parent_cx);
        parent_cx.with_span(span).span().end();
    }

    fn has_running_background_tasks_for_run(&self, run_key: &str) -> bool {
        self.current_background_tasks
            .lock()
            .values()
            .any(|active| active.run_key == run_key)
    }

    /// Resolve a (start, end) `SystemTime` pair for a span using its absolute
    /// `started_at_ms`/`ended_at_ms` when available, falling back to
    /// `now - duration_ms` for legacy payloads that only carry a duration.
    fn span_window(
        started_at_ms: u64,
        ended_at_ms: u64,
        duration_ms: u64,
    ) -> (std::time::SystemTime, std::time::SystemTime) {
        if started_at_ms != 0 && ended_at_ms >= started_at_ms {
            return (
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(started_at_ms),
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(ended_at_ms),
            );
        }
        let end_time = std::time::SystemTime::now();
        let start_time = end_time - std::time::Duration::from_millis(duration_ms);
        (start_time, end_time)
    }

    fn end_root_context_with_attrs(&self, run_key: &str, attrs: Vec<KeyValue>) {
        if let Some(cx) = self.root_contexts.lock().remove(run_key) {
            let span_ref = cx.span();
            span_ref.set_attributes(attrs);
            span_ref.end();
        }
    }

    fn defer_or_end_root_context(&self, run_key: &str, attrs: Vec<KeyValue>) {
        if self.has_running_background_tasks_for_run(run_key) {
            self.deferred_root_ends
                .lock()
                .insert(run_key.to_string(), attrs);
        } else {
            self.end_root_context_with_attrs(run_key, attrs);
        }
    }

    fn end_deferred_root_if_background_idle(&self, run_key: &str) {
        if self.has_running_background_tasks_for_run(run_key) {
            return;
        }
        let attrs = self.deferred_root_ends.lock().remove(run_key);
        if let Some(attrs) = attrs {
            self.end_pending_tool_spans_for_run(run_key);
            self.end_root_context_with_attrs(run_key, attrs);
        }
    }
}

/// Initialise an OTLP HTTP tracer from the given configuration.
///
/// Returns an `SdkTracerProvider` (caller should keep it alive) and an
/// `SdkTracer` suitable for passing to [`OtelMetricsSink::new`].
///
/// # Errors
///
/// Returns an error when no endpoint is configured or the OTLP exporter
/// fails to build.
pub fn init_otlp_tracer(
    config: &OtelConfig,
) -> Result<
    (opentelemetry_sdk::trace::SdkTracerProvider, SdkTracer),
    Box<dyn std::error::Error + Send + Sync>,
> {
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;

    let endpoint = config
        .effective_traces_endpoint()
        .ok_or("No OTLP endpoint configured")?;

    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()?;

    let mut resource_attrs = vec![];
    if let Some(name) = &config.service_name {
        resource_attrs.push(KeyValue::new("service.name", name.clone()));
    }
    if let Some(version) = &config.service_version {
        resource_attrs.push(KeyValue::new("service.version", version.clone()));
    }

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(Resource::builder().with_attributes(resource_attrs).build())
        .build();

    let tracer = provider.tracer("awaken");
    Ok((provider, tracer))
}

impl MetricsSink for OtelMetricsSink {
    fn record(&self, event: &MetricsEvent) {
        match event {
            MetricsEvent::Inference(span) => self.record_inference(span),
            MetricsEvent::Tool(span) => self.record_tool(span),
            MetricsEvent::Suspension(span) => self.record_suspension(span),
            MetricsEvent::Handoff(span) => self.record_handoff(span),
            MetricsEvent::Delegation(span) => self.record_delegation(span),
            MetricsEvent::EvaluationResult(event) => {
                OtelMetricsSink::record_evaluation_result(self, event);
            }
            MetricsEvent::BackgroundTask(span) => {
                OtelMetricsSink::record_background_task(self, span);
            }
        }
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        let run_keys = Self::run_keys_for_metrics(metrics);
        if run_keys.is_empty() {
            self.end_all_pending_tool_spans();
            self.end_all_current_inferences();
        } else {
            for run_key in &run_keys {
                if !self.has_running_background_tasks_for_run(run_key) {
                    self.end_pending_tool_spans_for_run(run_key);
                }
                self.end_current_inference(run_key);
            }
        }

        let agent_summary_attrs = vec![
            KeyValue::new(
                "gen_ai.usage.input_tokens",
                i64::from(metrics.total_input_tokens()),
            ),
            KeyValue::new(
                "gen_ai.usage.output_tokens",
                i64::from(metrics.total_output_tokens()),
            ),
            KeyValue::new(
                "awaken.session.inference_count",
                metrics.inference_count() as i64,
            ),
            KeyValue::new("awaken.session.tool_count", metrics.tool_count() as i64),
            KeyValue::new(
                "awaken.session.tool_failures",
                metrics.tool_failures() as i64,
            ),
            KeyValue::new(
                "awaken.session.duration",
                metrics.session_duration_ms as f64 / 1000.0,
            ),
        ];

        // End root agent spans created for this run. If metrics contain no
        // events, preserve the previous best-effort behavior and close all.
        if run_keys.is_empty() {
            for (_, cx) in self.root_contexts.lock().drain() {
                let span_ref = cx.span();
                span_ref.set_attributes(agent_summary_attrs.clone());
                span_ref.end();
            }
        } else {
            for run_key in run_keys {
                self.defer_or_end_root_context(&run_key, agent_summary_attrs.clone());
            }
        }
    }
}

impl Drop for OtelMetricsSink {
    fn drop(&mut self) {
        self.end_all_pending_tool_spans();
        self.end_all_current_inferences();
        for (_, active) in self.current_background_tasks.lock().drain() {
            active.cx.span().end();
        }
        for (_, cx) in self.root_contexts.lock().drain() {
            cx.span().end();
        }
    }
}

impl OtelMetricsSink {
    fn run_keys_for_metrics(metrics: &AgentMetrics) -> HashSet<String> {
        let mut run_keys = HashSet::new();
        for event in metrics.events() {
            let ctx = match &event {
                MetricsEvent::Inference(span) => &span.context,
                MetricsEvent::Tool(span) => &span.context,
                MetricsEvent::Suspension(span) => &span.context,
                MetricsEvent::Handoff(span) => &span.context,
                MetricsEvent::Delegation(span) => &span.context,
                MetricsEvent::EvaluationResult(event) => &event.context,
                MetricsEvent::BackgroundTask(span) => &span.context,
            };
            run_keys.insert(Self::run_key(ctx));
        }
        for event in &metrics.evaluations {
            run_keys.insert(Self::run_key(&event.context));
        }
        for span in &metrics.background_tasks {
            run_keys.insert(Self::run_key_for_background_context(&span.context));
        }
        run_keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricsEvent, SpanContext};
    use serde_json::json;
    use std::collections::HashMap;

    fn sample_genai_span() -> GenAISpan {
        GenAISpan {
            context: SpanContext::default(),
            step_index: None,
            model: "gpt-4".to_string(),
            provider: "openai".to_string(),
            operation: "chat".to_string(),
            response_model: Some("gpt-4-0125".to_string()),
            response_id: Some("chatcmpl-123".to_string()),
            finish_reasons: vec!["stop".to_string()],
            error_type: None,
            error_class: None,
            thinking_tokens: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: Some(150),
            cache_read_input_tokens: Some(20),
            cache_creation_input_tokens: None,
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_tokens: Some(4096),
            stop_sequences: Vec::new(),
            duration_ms: 1200,
            started_at_ms: 0,
            ended_at_ms: 0,
        }
    }

    fn sample_tool_span() -> ToolSpan {
        ToolSpan {
            context: SpanContext::default(),
            step_index: None,
            name: "read_file".to_string(),
            operation: "execute_tool".to_string(),
            call_id: "call_abc123".to_string(),
            tool_type: "function".to_string(),
            call_arguments: None,
            call_result: None,
            error_type: None,
            duration_ms: 50,
            started_at_ms: 0,
            ended_at_ms: 0,
        }
    }

    fn sample_background_task_span(
        status: awaken_runtime::extensions::background::TaskStatus,
    ) -> BackgroundTaskSpan {
        use awaken_runtime::extensions::background::TaskStatus;
        let completed_at_ms = if matches!(status, TaskStatus::Running) {
            None
        } else {
            Some(1_500)
        };
        BackgroundTaskSpan {
            context: SpanContext::default(),
            task_id: "bg_1".to_string(),
            task_type: "sub_agent".to_string(),
            task_name: Some("worker".to_string()),
            description: "background worker".to_string(),
            status,
            parent_task_id: None,
            error_message: None,
            created_at_ms: 1_000,
            completed_at_ms,
        }
    }

    #[test]
    fn genai_attributes_complete() {
        let span = sample_genai_span();
        let attrs = OtelMetricsSink::genai_attributes(&span);

        let attr_map: HashMap<&str, &KeyValue> =
            attrs.iter().map(|kv| (kv.key.as_str(), kv)).collect();

        assert!(attr_map.contains_key("gen_ai.provider.name"));
        assert!(attr_map.contains_key("gen_ai.request.model"));
        assert!(attr_map.contains_key("gen_ai.operation.name"));
        assert!(attr_map.contains_key("gen_ai.response.model"));
        assert!(attr_map.contains_key("gen_ai.response.id"));
        assert!(attr_map.contains_key("gen_ai.usage.input_tokens"));
        assert!(attr_map.contains_key("gen_ai.usage.output_tokens"));
        assert!(attr_map.contains_key("gen_ai.usage.cache_read.input_tokens"));
        assert!(attr_map.contains_key("gen_ai.request.temperature"));
        assert!(attr_map.contains_key("gen_ai.request.top_p"));
        assert!(attr_map.contains_key("gen_ai.request.max_tokens"));
    }

    #[test]
    fn genai_attributes_minimal() {
        let span = GenAISpan {
            context: SpanContext::default(),
            step_index: None,
            model: "claude-3".to_string(),
            provider: "anthropic".to_string(),
            operation: "chat".to_string(),
            response_model: None,
            response_id: None,
            finish_reasons: Vec::new(),
            error_type: None,
            error_class: None,
            thinking_tokens: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: Vec::new(),
            duration_ms: 100,
            started_at_ms: 0,
            ended_at_ms: 0,
        };
        let attrs = OtelMetricsSink::genai_attributes(&span);

        // Should have the required GenAI span attributes available to Awaken.
        assert!(attrs.len() >= 3); // provider, model, operation
        assert!(
            !attrs
                .iter()
                .any(|kv| kv.key.as_str() == "gen_ai.response.model")
        );
    }

    #[test]
    fn genai_attributes_with_error() {
        let span = GenAISpan {
            error_type: Some("rate_limit".to_string()),
            ..sample_genai_span()
        };
        let attrs = OtelMetricsSink::genai_attributes(&span);
        assert!(attrs.iter().any(|kv| kv.key.as_str() == "error.type"));
    }

    #[test]
    fn tool_attributes_success() {
        let span = sample_tool_span();
        let attrs = OtelMetricsSink::tool_attributes(&span);

        let attr_map: HashMap<&str, &KeyValue> =
            attrs.iter().map(|kv| (kv.key.as_str(), kv)).collect();

        assert!(attr_map.contains_key("gen_ai.tool.name"));
        assert!(attr_map.contains_key("gen_ai.operation.name"));
        assert!(attr_map.contains_key("gen_ai.tool.call.id"));
        assert!(attr_map.contains_key("gen_ai.tool.type"));
        assert!(!attr_map.contains_key("error.type"));
    }

    #[test]
    fn tool_attributes_with_error() {
        let span = ToolSpan {
            error_type: Some("permission_denied".to_string()),
            ..sample_tool_span()
        };
        let attrs = OtelMetricsSink::tool_attributes(&span);
        assert!(attrs.iter().any(|kv| kv.key.as_str() == "error.type"));
    }

    #[test]
    fn tool_attributes_include_opt_in_payloads_as_json() {
        let span = ToolSpan {
            call_arguments: Some(json!({"query": "otel", "limit": 3})),
            call_result: Some(json!({"count": 1, "source": "docs"})),
            ..sample_tool_span()
        };
        let attrs = OtelMetricsSink::tool_attributes(&span);

        let arguments =
            kv_string(&attrs, "gen_ai.tool.call.arguments").expect("tool arguments attribute");
        let result = kv_string(&attrs, "gen_ai.tool.call.result").expect("tool result attribute");

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(arguments).expect("valid arguments json"),
            json!({"query": "otel", "limit": 3})
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(result).expect("valid result json"),
            json!({"count": 1, "source": "docs"})
        );
    }

    #[test]
    fn otel_sink_with_noop_tracer() {
        use opentelemetry::trace::TracerProvider;
        use opentelemetry_sdk::trace::SdkTracerProvider;

        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("test");
        let sink = OtelMetricsSink::new(tracer);

        // Should not panic with noop spans
        sink.record(&MetricsEvent::Inference(sample_genai_span()));
        sink.record(&MetricsEvent::Tool(sample_tool_span()));
        sink.on_run_end(&AgentMetrics {
            inferences: vec![sample_genai_span()],
            tools: vec![sample_tool_span()],
            session_duration_ms: 5000,
            ..Default::default()
        });
    }

    // ── In-memory span exporter for OTLP pipeline verification ────────

    /// A simple in-memory span exporter that captures exported spans for
    /// test assertions. Uses `Arc<Mutex<Vec<SpanData>>>` so the test can
    /// read back the spans after the provider flushes.
    mod capture {
        use futures_util::future::BoxFuture;
        use opentelemetry_sdk::error::OTelSdkResult;
        use opentelemetry_sdk::trace::{SpanData, SpanExporter};
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Debug)]
        pub struct InMemorySpanExporter {
            spans: Arc<Mutex<Vec<SpanData>>>,
        }

        impl InMemorySpanExporter {
            pub fn new() -> Self {
                Self {
                    spans: Arc::new(Mutex::new(Vec::new())),
                }
            }

            pub fn finished_spans(&self) -> Vec<SpanData> {
                self.spans.lock().unwrap().clone()
            }
        }

        impl SpanExporter for InMemorySpanExporter {
            fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, OTelSdkResult> {
                self.spans.lock().unwrap().extend(batch);
                Box::pin(std::future::ready(Ok(())))
            }
        }
    }

    /// Build an OtelMetricsSink backed by our in-memory exporter so
    /// exported OTel spans can be inspected.
    fn make_capturing_sink() -> (
        OtelMetricsSink,
        capture::InMemorySpanExporter,
        opentelemetry_sdk::trace::SdkTracerProvider,
    ) {
        use opentelemetry::trace::TracerProvider;
        use opentelemetry_sdk::trace::SdkTracerProvider;

        let exporter = capture::InMemorySpanExporter::new();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("awaken-test");
        let sink = OtelMetricsSink::new(tracer);
        (sink, exporter, provider)
    }

    /// Helper: build a HashMap of attribute key -> Value from a SpanData.
    fn attr_map(
        span: &opentelemetry_sdk::trace::SpanData,
    ) -> HashMap<String, opentelemetry::Value> {
        span.attributes
            .iter()
            .map(|kv| (kv.key.to_string(), kv.value.clone()))
            .collect()
    }

    fn kv_string<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a str> {
        attrs
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .and_then(|kv| match &kv.value {
                opentelemetry::Value::String(value) => Some(value.as_str()),
                _ => None,
            })
    }

    // ── OTLP pipeline span verification tests ────────────────────────

    #[test]
    fn otlp_genai_span_has_all_required_attributes() {
        let (sink, exporter, provider) = make_capturing_sink();

        let span = GenAISpan {
            context: SpanContext {
                run_id: "run-42".to_string(),
                thread_id: "thread-7".to_string(),
                agent_id: "agent-alpha".to_string(),
                parent_run_id: None,
                parent_tool_call_id: None,
            },
            step_index: Some(3),
            duration_ms: 1200,
            started_at_ms: 0,
            ended_at_ms: 0,
            ..sample_genai_span()
        };

        sink.record(&MetricsEvent::Inference(span));

        // on_run_end ends the root agent span.
        sink.on_run_end(&AgentMetrics {
            inferences: vec![sample_genai_span()],
            ..Default::default()
        });

        // Force the provider to flush so SimpleSpanProcessor exports.
        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        // 1 inference + 1 agent root = 2
        assert_eq!(spans.len(), 2, "expected 2 exported spans");

        let exported = spans
            .iter()
            .find(|s| s.name.as_ref() == "chat gpt-4")
            .expect("inference span not found");

        // SpanKind
        assert_eq!(exported.span_kind, opentelemetry::trace::SpanKind::Client);

        // The inference span has a parent (the auto-created root agent).
        assert!(
            exported.parent_span_id != opentelemetry::trace::SpanId::INVALID,
            "inference span should have a parent (the root agent span)"
        );

        // Verify duration is approximately correct (>= 1s).
        let duration = exported
            .end_time
            .duration_since(exported.start_time)
            .expect("end > start");
        assert!(
            duration >= std::time::Duration::from_millis(1000),
            "span duration should be >= 1s, got {duration:?}"
        );

        // Attributes
        let attrs = attr_map(exported);
        assert_eq!(
            attrs.get("gen_ai.provider.name").map(|v| v.to_string()),
            Some("openai".to_string())
        );
        assert_eq!(
            attrs.get("gen_ai.request.model").map(|v| v.to_string()),
            Some("gpt-4".to_string())
        );
        assert_eq!(
            attrs
                .get("gen_ai.usage.input_tokens")
                .map(|v| v.to_string()),
            Some("100".to_string())
        );
        assert_eq!(
            attrs
                .get("gen_ai.usage.output_tokens")
                .map(|v| v.to_string()),
            Some("50".to_string())
        );
        assert_eq!(
            attrs.get("awaken.run.id").map(|v| v.to_string()),
            Some("run-42".to_string())
        );
        assert_eq!(
            attrs.get("awaken.thread.id").map(|v| v.to_string()),
            Some("thread-7".to_string())
        );
        assert_eq!(
            attrs.get("awaken.agent.id").map(|v| v.to_string()),
            Some("agent-alpha".to_string())
        );
        assert_eq!(
            attrs.get("awaken.step.index").map(|v| v.to_string()),
            Some("3".to_string())
        );
    }

    #[test]
    fn otlp_tool_span_has_all_required_attributes() {
        let (sink, exporter, provider) = make_capturing_sink();

        let context = SpanContext {
            run_id: "run-42".to_string(),
            thread_id: "thread-7".to_string(),
            agent_id: "agent-alpha".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        };

        // Record an inference first so the tool becomes its child.
        sink.record(&MetricsEvent::Inference(GenAISpan {
            context: context.clone(),
            ..sample_genai_span()
        }));

        let span = ToolSpan {
            context,
            step_index: Some(1),
            ..sample_tool_span()
        };

        sink.record(&MetricsEvent::Tool(span));

        sink.on_run_end(&AgentMetrics::default());
        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        // 1 inference + 1 tool + 1 agent = 3
        assert_eq!(spans.len(), 3, "expected 3 exported spans");

        let exported = spans
            .iter()
            .find(|s| s.name.as_ref() == "execute_tool read_file")
            .expect("tool span not found");

        // SpanKind
        assert_eq!(exported.span_kind, opentelemetry::trace::SpanKind::Internal);

        // Tool span has a parent (the inference span).
        assert!(
            exported.parent_span_id != opentelemetry::trace::SpanId::INVALID,
            "tool span should have a parent"
        );
        let inference = spans
            .iter()
            .find(|s| s.name.as_ref() == "chat gpt-4")
            .expect("inference span not found");
        assert_eq!(
            exported.parent_span_id,
            inference.span_context.span_id(),
            "tool span should be parented to the active inference for the same run"
        );

        // Attributes
        let attrs = attr_map(exported);
        assert_eq!(
            attrs.get("gen_ai.tool.call.id").map(|v| v.to_string()),
            Some("call_abc123".to_string())
        );
        assert_eq!(
            attrs.get("gen_ai.tool.name").map(|v| v.to_string()),
            Some("read_file".to_string())
        );
        assert_eq!(
            attrs.get("awaken.run.id").map(|v| v.to_string()),
            Some("run-42".to_string())
        );
        assert_eq!(
            attrs.get("awaken.thread.id").map(|v| v.to_string()),
            Some("thread-7".to_string())
        );
        assert_eq!(
            attrs.get("awaken.agent.id").map(|v| v.to_string()),
            Some("agent-alpha".to_string())
        );
        assert_eq!(
            attrs.get("awaken.step.index").map(|v| v.to_string()),
            Some("1".to_string())
        );
    }

    #[test]
    fn otlp_delegation_span_references_agent_tool_and_child_run() {
        let (sink, exporter, provider) = make_capturing_sink();

        let context = SpanContext {
            run_id: "run-delegate".to_string(),
            thread_id: "thread-delegate".to_string(),
            agent_id: "agent-orchestrator".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        };
        let inference = GenAISpan {
            context: context.clone(),
            ..sample_genai_span()
        };
        let tool = ToolSpan {
            context: context.clone(),
            name: "agent_run_worker".to_string(),
            call_id: "call-delegate".to_string(),
            ..sample_tool_span()
        };
        let delegation = DelegationSpan {
            context: context.clone(),
            parent_run_id: "run-delegate".to_string(),
            child_run_id: Some("child-run-delegate".to_string()),
            target_agent_id: "worker".to_string(),
            tool_call_id: "call-delegate".to_string(),
            duration_ms: Some(125),
            success: true,
            error_message: None,
            timestamp_ms: 0,
        };

        sink.record(&MetricsEvent::Inference(inference.clone()));
        sink.record(&MetricsEvent::Tool(tool.clone()));
        sink.record(&MetricsEvent::Delegation(delegation.clone()));
        sink.on_run_end(&AgentMetrics {
            inferences: vec![inference],
            tools: vec![tool],
            delegations: vec![delegation],
            ..Default::default()
        });

        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        assert_eq!(
            spans.len(),
            4,
            "expected agent, inference, tool, delegation"
        );

        let agent = spans
            .iter()
            .find(|s| s.name.starts_with("invoke_agent"))
            .expect("agent span not found");
        let tool = spans
            .iter()
            .find(|s| s.name.as_ref() == "execute_tool agent_run_worker")
            .expect("agent tool span not found");
        let delegation = spans
            .iter()
            .find(|s| s.name.as_ref() == "awaken.delegation")
            .expect("delegation span not found");

        assert_eq!(
            tool.span_context.trace_id(),
            agent.span_context.trace_id(),
            "agent tool span should be in the parent agent trace"
        );
        assert_eq!(
            delegation.span_context.trace_id(),
            agent.span_context.trace_id(),
            "delegation span should be in the parent agent trace"
        );
        assert_ne!(
            delegation.parent_span_id,
            opentelemetry::trace::SpanId::INVALID,
            "delegation span should keep a parent in the trace tree"
        );

        let tool_attrs = attr_map(tool);
        assert_eq!(
            tool_attrs.get("gen_ai.tool.call.id").map(|v| v.to_string()),
            Some("call-delegate".to_string())
        );
        assert_eq!(
            tool_attrs.get("gen_ai.tool.name").map(|v| v.to_string()),
            Some("agent_run_worker".to_string())
        );

        let delegation_attrs = attr_map(delegation);
        assert_eq!(
            delegation_attrs
                .get("awaken.delegation.parent_run_id")
                .map(|v| v.to_string()),
            Some("run-delegate".to_string())
        );
        assert_eq!(
            delegation_attrs
                .get("awaken.delegation.child_run_id")
                .map(|v| v.to_string()),
            Some("child-run-delegate".to_string())
        );
        assert_eq!(
            delegation_attrs
                .get("awaken.delegation.target_agent_id")
                .map(|v| v.to_string()),
            Some("worker".to_string())
        );
        assert_eq!(
            delegation_attrs
                .get("gen_ai.tool.call.id")
                .map(|v| v.to_string()),
            Some("call-delegate".to_string())
        );
        assert_eq!(
            delegation_attrs
                .get("awaken.delegation.success")
                .map(|v| v.to_string()),
            Some("true".to_string())
        );
    }

    #[test]
    fn otlp_background_task_span_attaches_to_parent_tool_context() {
        let (sink, exporter, provider) = make_capturing_sink();

        let context = SpanContext {
            run_id: "run-bg".to_string(),
            thread_id: "thread-bg".to_string(),
            agent_id: "agent-bg".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        };
        let parent_inference = GenAISpan {
            context: context.clone(),
            model: "parent-model".to_string(),
            ..sample_genai_span()
        };
        let tool = ToolSpan {
            context: context.clone(),
            name: "spawn_background".to_string(),
            call_id: "call-bg".to_string(),
            ..sample_tool_span()
        };
        let background_context = SpanContext {
            parent_tool_call_id: Some("call-bg".to_string()),
            ..context.clone()
        };
        let running = BackgroundTaskSpan {
            context: background_context.clone(),
            ..sample_background_task_span(
                awaken_runtime::extensions::background::TaskStatus::Running,
            )
        };
        let completed = BackgroundTaskSpan {
            context: background_context,
            status: awaken_runtime::extensions::background::TaskStatus::Completed,
            completed_at_ms: Some(1_500),
            ..running.clone()
        };

        sink.record(&MetricsEvent::Inference(parent_inference.clone()));
        sink.record(&MetricsEvent::Tool(tool.clone()));
        sink.record_background_task(&running);
        sink.record_background_task(&completed);
        sink.on_run_end(&AgentMetrics {
            inferences: vec![parent_inference],
            tools: vec![tool],
            background_tasks: vec![completed],
            session_duration_ms: 600,
            ..Default::default()
        });

        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        let tool = spans
            .iter()
            .find(|s| s.name.as_ref() == "execute_tool spawn_background")
            .expect("background spawning tool span not found");
        let background = spans
            .iter()
            .find(|s| s.name.as_ref() == "awaken.background_task")
            .expect("background task span not found");

        assert_eq!(
            background.span_context.trace_id(),
            tool.span_context.trace_id(),
            "background task should remain in the parent tool trace"
        );
        assert_eq!(
            background.parent_span_id,
            tool.span_context.span_id(),
            "background task should be parented to the tool call that spawned it"
        );
        let attrs = attr_map(background);
        assert_eq!(
            attrs
                .get("awaken.background_task.status")
                .map(|v| v.to_string()),
            Some("completed".to_string())
        );
        assert_eq!(
            attrs
                .get("awaken.background_task.parent_tool_call_id")
                .map(|v| v.to_string()),
            Some("call-bg".to_string())
        );
    }

    #[test]
    fn otlp_background_task_before_tool_uses_real_tool_parent_and_duration() {
        let (sink, exporter, provider) = make_capturing_sink();

        let context = SpanContext {
            run_id: "run-bg-early".to_string(),
            thread_id: "thread-bg".to_string(),
            agent_id: "agent-bg".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        };
        let parent_inference = GenAISpan {
            context: context.clone(),
            model: "parent-model".to_string(),
            ..sample_genai_span()
        };
        let tool = ToolSpan {
            context: context.clone(),
            name: "spawn_background".to_string(),
            call_id: "call-bg-early".to_string(),
            duration_ms: 250,
            started_at_ms: 0,
            ended_at_ms: 0,
            ..sample_tool_span()
        };
        let background_context = SpanContext {
            parent_tool_call_id: Some(tool.call_id.clone()),
            ..context.clone()
        };
        let running = BackgroundTaskSpan {
            context: background_context.clone(),
            task_id: "bg-early".to_string(),
            ..sample_background_task_span(
                awaken_runtime::extensions::background::TaskStatus::Running,
            )
        };
        let completed = BackgroundTaskSpan {
            context: background_context,
            status: awaken_runtime::extensions::background::TaskStatus::Completed,
            completed_at_ms: Some(1_500),
            ..running.clone()
        };

        sink.record(&MetricsEvent::Inference(parent_inference.clone()));
        sink.record_background_task(&running);
        sink.record(&MetricsEvent::Tool(tool.clone()));
        sink.record_background_task(&completed);
        sink.on_run_end(&AgentMetrics {
            inferences: vec![parent_inference],
            tools: vec![tool],
            background_tasks: vec![completed],
            session_duration_ms: 600,
            ..Default::default()
        });

        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        let tool = spans
            .iter()
            .find(|s| s.name.as_ref() == "execute_tool spawn_background")
            .expect("background spawning tool span not found");
        let background = spans
            .iter()
            .find(|s| s.name.as_ref() == "awaken.background_task")
            .expect("background task span not found");

        assert_eq!(
            background.parent_span_id,
            tool.span_context.span_id(),
            "early background event should be reparented to the eventual tool span id"
        );
        let tool_duration = tool
            .end_time
            .duration_since(tool.start_time)
            .expect("tool end after start");
        assert_eq!(
            tool_duration,
            std::time::Duration::from_millis(250),
            "lazy tool completion should preserve ToolSpan duration"
        );
    }

    #[test]
    fn otlp_run_end_defers_root_until_running_background_task_finishes() {
        let (sink, exporter, provider) = make_capturing_sink();

        let context = SpanContext {
            run_id: "run-bg-open".to_string(),
            thread_id: "thread-bg".to_string(),
            agent_id: "agent-bg".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        };
        let inference = GenAISpan {
            context: context.clone(),
            model: "parent-model".to_string(),
            ..sample_genai_span()
        };
        let tool = ToolSpan {
            context: context.clone(),
            name: "spawn_background".to_string(),
            call_id: "call-bg-open".to_string(),
            ..sample_tool_span()
        };
        let background_context = SpanContext {
            parent_tool_call_id: Some(tool.call_id.clone()),
            ..context.clone()
        };
        let running = BackgroundTaskSpan {
            context: background_context.clone(),
            task_id: "bg-open".to_string(),
            ..sample_background_task_span(
                awaken_runtime::extensions::background::TaskStatus::Running,
            )
        };
        let completed = BackgroundTaskSpan {
            context: background_context,
            status: awaken_runtime::extensions::background::TaskStatus::Completed,
            completed_at_ms: Some(1_500),
            ..running.clone()
        };

        sink.record(&MetricsEvent::Inference(inference.clone()));
        sink.record(&MetricsEvent::Tool(tool.clone()));
        sink.record_background_task(&running);
        sink.on_run_end(&AgentMetrics {
            inferences: vec![inference.clone()],
            tools: vec![tool.clone()],
            background_tasks: vec![running],
            session_duration_ms: 600,
            ..Default::default()
        });

        assert!(
            exporter
                .finished_spans()
                .iter()
                .all(|s| !s.name.starts_with("invoke_agent")),
            "root span should remain open while background task is running"
        );

        sink.record_background_task(&completed);
        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        let root = spans
            .iter()
            .find(|s| s.name.as_ref() == "invoke_agent agent-bg")
            .expect("root span should close after background task terminal event");
        let background = spans
            .iter()
            .find(|s| s.name.as_ref() == "awaken.background_task")
            .expect("background task span not found");
        assert_eq!(
            background.span_context.trace_id(),
            root.span_context.trace_id()
        );
    }

    #[tokio::test]
    async fn otlp_background_subagent_run_is_parented_to_background_task_span() {
        use std::sync::Arc;

        use awaken_runtime::extensions::background::{
            BackgroundTaskManager, BackgroundTaskPlugin, TaskParentContext, TaskResult,
        };

        let (sink, exporter, provider) = make_capturing_sink();
        let sink = Arc::new(sink);
        let store = awaken_runtime::StateStore::new();
        let manager = Arc::new(BackgroundTaskManager::new());
        manager.set_store(store.clone());
        store
            .install_plugin(BackgroundTaskPlugin::new(manager.clone()))
            .expect("background keys should register");

        let parent_context = SpanContext {
            run_id: "run-bg-subagent".to_string(),
            thread_id: "thread-bg-subagent".to_string(),
            agent_id: "agent-bg".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        };
        let parent_inference = GenAISpan {
            context: parent_context.clone(),
            model: "parent-model".to_string(),
            ..sample_genai_span()
        };
        let tool = ToolSpan {
            context: parent_context.clone(),
            name: "spawn_background".to_string(),
            call_id: "call-bg-subagent".to_string(),
            ..sample_tool_span()
        };
        let child_context = SpanContext {
            run_id: "child-bg-run".to_string(),
            thread_id: "child-bg-run".to_string(),
            agent_id: "worker".to_string(),
            parent_run_id: Some(parent_context.run_id.clone()),
            parent_tool_call_id: Some(tool.call_id.clone()),
        };
        let child_inference = GenAISpan {
            context: child_context.clone(),
            model: "child-model".to_string(),
            ..sample_genai_span()
        };
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        sink.record(&MetricsEvent::Inference(parent_inference.clone()));
        let task_id = manager
            .spawn(
                "thread-bg-subagent",
                "sub_agent",
                Some("worker"),
                "worker agent",
                TaskParentContext {
                    run_id: Some(parent_context.run_id.clone()),
                    call_id: Some(tool.call_id.clone()),
                    agent_id: Some(parent_context.agent_id.clone()),
                    task_id: None,
                },
                {
                    let sink = sink.clone();
                    let child_inference = child_inference.clone();
                    move |_ctx| async move {
                        // The child agent run can be observed before the spawning
                        // tool span and background lifecycle hook are emitted; it
                        // should still be nested as Tool -> BackgroundTask ->
                        // invoke_agent by reading the runtime task-local context.
                        sink.record(&MetricsEvent::Inference(child_inference));
                        let _ = done_tx.send(());
                        TaskResult::Success(json!({}))
                    }
                },
            )
            .await
            .expect("background sub-agent task should spawn");
        done_rx
            .await
            .expect("background child inference should be recorded");

        sink.on_run_end(&AgentMetrics {
            inferences: vec![child_inference.clone()],
            session_duration_ms: 300,
            ..Default::default()
        });
        let completed = BackgroundTaskSpan {
            context: SpanContext {
                parent_tool_call_id: Some(tool.call_id.clone()),
                ..parent_context.clone()
            },
            task_id: task_id.clone(),
            status: awaken_runtime::extensions::background::TaskStatus::Completed,
            completed_at_ms: Some(1_500),
            ..sample_background_task_span(
                awaken_runtime::extensions::background::TaskStatus::Completed,
            )
        };
        sink.record(&MetricsEvent::Tool(tool.clone()));
        sink.record_background_task(&completed);
        sink.on_run_end(&AgentMetrics {
            inferences: vec![parent_inference],
            tools: vec![tool],
            background_tasks: vec![completed],
            session_duration_ms: 900,
            ..Default::default()
        });

        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        let tool = spans
            .iter()
            .find(|s| s.name.as_ref() == "execute_tool spawn_background")
            .expect("background spawning tool span not found");
        let background = spans
            .iter()
            .filter(|s| s.name.as_ref() == "awaken.background_task")
            .collect::<Vec<_>>();
        assert_eq!(
            background.len(),
            1,
            "background task lineage should use one task span"
        );
        let background = background[0];
        let child_agent = spans
            .iter()
            .find(|s| s.name.as_ref() == "invoke_agent worker")
            .expect("child agent root span not found");
        let child_chat = spans
            .iter()
            .find(|s| s.name.as_ref() == "chat child-model")
            .expect("child chat span not found");

        assert_eq!(
            background.span_context.trace_id(),
            tool.span_context.trace_id()
        );
        assert_eq!(
            child_agent.span_context.trace_id(),
            tool.span_context.trace_id()
        );
        assert_eq!(
            background.parent_span_id,
            tool.span_context.span_id(),
            "background task should stay under the spawning tool"
        );
        assert_eq!(
            child_agent.parent_span_id,
            background.span_context.span_id(),
            "background sub-agent root should be parented to the task span"
        );
        assert_eq!(
            child_chat.parent_span_id,
            child_agent.span_context.span_id(),
            "child chat should remain under the child agent root"
        );

        let child_attrs = attr_map(child_agent);
        assert_eq!(
            child_attrs
                .get("awaken.parent_task.id")
                .map(|v| v.to_string()),
            Some(task_id)
        );
    }

    #[test]
    fn otlp_subagent_invoke_agent_inherits_parent_run_context() {
        let (sink, exporter, provider) = make_capturing_sink();

        let parent_context = SpanContext {
            run_id: "parent-run".to_string(),
            thread_id: "thread-delegate".to_string(),
            agent_id: "orchestrator".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        };
        let child_context = SpanContext {
            run_id: "child-run".to_string(),
            thread_id: "child-run".to_string(),
            agent_id: "worker".to_string(),
            parent_run_id: Some("parent-run".to_string()),
            parent_tool_call_id: None,
        };
        let parent_inference = GenAISpan {
            context: parent_context.clone(),
            model: "parent-model".to_string(),
            ..sample_genai_span()
        };
        let child_inference = GenAISpan {
            context: child_context.clone(),
            model: "child-model".to_string(),
            ..sample_genai_span()
        };

        sink.record(&MetricsEvent::Inference(parent_inference.clone()));
        sink.record(&MetricsEvent::Inference(child_inference.clone()));
        sink.on_run_end(&AgentMetrics {
            inferences: vec![child_inference],
            session_duration_ms: 250,
            ..Default::default()
        });
        sink.on_run_end(&AgentMetrics {
            inferences: vec![parent_inference],
            session_duration_ms: 500,
            ..Default::default()
        });

        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        let parent_agent = spans
            .iter()
            .find(|s| s.name.as_ref() == "invoke_agent orchestrator")
            .expect("parent agent span not found");
        let parent_chat = spans
            .iter()
            .find(|s| s.name.as_ref() == "chat parent-model")
            .expect("parent inference span not found");
        let child_agent = spans
            .iter()
            .find(|s| s.name.as_ref() == "invoke_agent worker")
            .expect("child agent span not found");
        let child_chat = spans
            .iter()
            .find(|s| s.name.as_ref() == "chat child-model")
            .expect("child inference span not found");

        assert_eq!(
            child_agent.span_context.trace_id(),
            parent_agent.span_context.trace_id(),
            "child invoke_agent should share the parent trace id"
        );
        assert_eq!(
            child_agent.parent_span_id,
            parent_chat.span_context.span_id(),
            "child invoke_agent should inherit the parent run's active inference context"
        );
        assert_eq!(
            child_chat.parent_span_id,
            child_agent.span_context.span_id(),
            "child inference should remain under the child invoke_agent span"
        );

        let child_agent_attrs = attr_map(child_agent);
        assert_eq!(
            child_agent_attrs
                .get("awaken.parent_run.id")
                .map(|v| v.to_string()),
            Some("parent-run".to_string())
        );
    }

    #[test]
    fn otlp_run_end_closes_agent_span() {
        let (sink, exporter, provider) = make_capturing_sink();

        // Record some events first.
        sink.record(&MetricsEvent::Inference(sample_genai_span()));
        sink.record(&MetricsEvent::Tool(sample_tool_span()));

        // Now fire on_run_end with aggregate metrics.
        let metrics = AgentMetrics {
            inferences: vec![sample_genai_span()],
            tools: vec![sample_tool_span()],
            session_duration_ms: 8000,
            ..Default::default()
        };
        sink.on_run_end(&metrics);

        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        // 1 inference + 1 tool + 1 agent = 3 spans
        assert_eq!(spans.len(), 3, "expected 3 exported spans");

        // Find the agent span.
        let agent = spans
            .iter()
            .find(|s| s.name.starts_with("invoke_agent"))
            .expect("agent span not found");

        assert_eq!(agent.span_kind, opentelemetry::trace::SpanKind::Internal);

        // Agent span is the root — no parent.
        assert_eq!(
            agent.parent_span_id,
            opentelemetry::trace::SpanId::INVALID,
            "agent span should be the root (no parent)"
        );

        // All other spans share the same trace_id as the session.
        let trace_id = agent.span_context.trace_id();
        for s in &spans {
            assert_eq!(
                s.span_context.trace_id(),
                trace_id,
                "span '{}' should share trace_id with agent",
                s.name
            );
        }

        // Inference and tool spans should be children (have a parent).
        let inference = spans
            .iter()
            .find(|s| s.name.starts_with("chat"))
            .expect("inference span not found");
        assert_eq!(
            inference.parent_span_id,
            agent.span_context.span_id(),
            "inference span should be a child of the agent"
        );

        let tool = spans
            .iter()
            .find(|s| s.name.starts_with("execute_tool"))
            .expect("tool span not found");
        assert_eq!(
            tool.parent_span_id,
            inference.span_context.span_id(),
            "tool span should be a child of the inference"
        );

        let attrs = attr_map(agent);
        assert_eq!(
            attrs.get("gen_ai.operation.name").map(|v| v.to_string()),
            Some("invoke_agent".to_string())
        );
        assert_eq!(
            attrs.get("gen_ai.provider.name").map(|v| v.to_string()),
            Some("openai".to_string())
        );
        assert_eq!(
            attrs
                .get("gen_ai.usage.input_tokens")
                .map(|v| v.to_string()),
            Some("100".to_string())
        );
        assert_eq!(
            attrs
                .get("gen_ai.usage.output_tokens")
                .map(|v| v.to_string()),
            Some("50".to_string())
        );
        assert_eq!(
            attrs
                .get("awaken.session.inference_count")
                .map(|v| v.to_string()),
            Some("1".to_string())
        );
        assert_eq!(
            attrs
                .get("awaken.session.tool_count")
                .map(|v| v.to_string()),
            Some("1".to_string())
        );
        assert_eq!(
            attrs
                .get("awaken.session.tool_failures")
                .map(|v| v.to_string()),
            Some("0".to_string())
        );
        assert!(attrs.contains_key("awaken.session.duration"));
    }

    #[test]
    fn otlp_internal_events_do_not_overwrite_agent_provider() {
        let (sink, exporter, provider) = make_capturing_sink();

        let context = SpanContext {
            run_id: "run-provider".to_string(),
            thread_id: "thread-provider".to_string(),
            agent_id: "agent-provider".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        };
        sink.record(&MetricsEvent::Inference(GenAISpan {
            context: context.clone(),
            provider: "openai".to_string(),
            ..sample_genai_span()
        }));
        sink.record(&MetricsEvent::Handoff(HandoffSpan {
            context,
            from_agent_id: "agent-provider".to_string(),
            to_agent_id: "agent-next".to_string(),
            reason: Some("handoff".to_string()),
            timestamp_ms: 0,
        }));
        sink.on_run_end(&AgentMetrics::default());

        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        let agent = spans
            .iter()
            .find(|s| s.name.starts_with("invoke_agent"))
            .expect("agent span not found");
        let attrs = attr_map(agent);
        assert_eq!(
            attrs.get("gen_ai.provider.name").map(|v| v.to_string()),
            Some("openai".to_string())
        );
    }

    #[test]
    fn otlp_evaluation_result_event_is_parented_to_active_inference() {
        let (sink, exporter, provider) = make_capturing_sink();

        let context = SpanContext {
            run_id: "run-eval".to_string(),
            thread_id: "thread-eval".to_string(),
            agent_id: "agent-eval".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        };
        let inference = GenAISpan {
            context: context.clone(),
            response_id: Some("chatcmpl-eval".to_string()),
            ..sample_genai_span()
        };

        sink.record(&MetricsEvent::Inference(inference.clone()));
        sink.record_evaluation_result(&EvaluationResultEvent {
            context,
            name: "faithfulness".to_string(),
            score_label: Some("pass".to_string()),
            score_value: Some(1.0),
            explanation: Some("answer is grounded".to_string()),
            response_id: Some("chatcmpl-eval".to_string()),
            error_type: None,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_millis() as u64,
        });
        sink.on_run_end(&AgentMetrics {
            inferences: vec![inference],
            ..Default::default()
        });

        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        let inference_span = spans
            .iter()
            .find(|s| s.name.as_ref() == "chat gpt-4")
            .expect("inference span not found");
        let event = inference_span
            .events
            .iter()
            .find(|event| event.name.as_ref() == "gen_ai.evaluation.result")
            .expect("evaluation event not found");

        assert_eq!(
            kv_string(&event.attributes, "gen_ai.evaluation.name"),
            Some("faithfulness")
        );
        assert_eq!(
            kv_string(&event.attributes, "gen_ai.evaluation.score.label"),
            Some("pass")
        );
        assert_eq!(
            kv_string(&event.attributes, "gen_ai.evaluation.explanation"),
            Some("answer is grounded")
        );
        assert_eq!(
            kv_string(&event.attributes, "gen_ai.response.id"),
            Some("chatcmpl-eval")
        );
        let event_attrs: HashMap<String, opentelemetry::Value> = event
            .attributes
            .iter()
            .map(|kv| (kv.key.to_string(), kv.value.clone()))
            .collect();
        assert_eq!(
            event_attrs
                .get("gen_ai.evaluation.score.value")
                .map(|v| v.to_string()),
            Some("1".to_string())
        );
        assert_eq!(
            kv_string(&event.attributes, "awaken.run.id"),
            Some("run-eval")
        );
    }

    #[test]
    fn otlp_multi_step_creates_correlated_spans() {
        let (sink, exporter, provider) = make_capturing_sink();

        let ctx = SpanContext {
            run_id: "run-99".to_string(),
            thread_id: "thread-1".to_string(),
            agent_id: "agent-beta".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        };

        // Step 0: inference + 2 tools
        sink.record(&MetricsEvent::Inference(GenAISpan {
            context: ctx.clone(),
            step_index: Some(0),
            model: "gpt-4".to_string(),
            ..sample_genai_span()
        }));
        sink.record(&MetricsEvent::Tool(ToolSpan {
            context: ctx.clone(),
            step_index: Some(0),
            name: "search".to_string(),
            call_id: "call_1".to_string(),
            ..sample_tool_span()
        }));
        sink.record(&MetricsEvent::Tool(ToolSpan {
            context: ctx.clone(),
            step_index: Some(0),
            name: "read".to_string(),
            call_id: "call_2".to_string(),
            ..sample_tool_span()
        }));

        // Step 1: inference + 1 tool
        sink.record(&MetricsEvent::Inference(GenAISpan {
            context: ctx.clone(),
            step_index: Some(1),
            model: "gpt-4".to_string(),
            ..sample_genai_span()
        }));
        sink.record(&MetricsEvent::Tool(ToolSpan {
            context: ctx.clone(),
            step_index: Some(1),
            name: "write".to_string(),
            call_id: "call_3".to_string(),
            ..sample_tool_span()
        }));

        sink.on_run_end(&AgentMetrics {
            inferences: vec![
                GenAISpan {
                    context: ctx.clone(),
                    step_index: Some(0),
                    model: "gpt-4".to_string(),
                    ..sample_genai_span()
                },
                GenAISpan {
                    context: ctx.clone(),
                    step_index: Some(1),
                    model: "gpt-4".to_string(),
                    ..sample_genai_span()
                },
            ],
            tools: vec![sample_tool_span(), sample_tool_span(), sample_tool_span()],
            session_duration_ms: 5000,
            ..Default::default()
        });

        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        // 2 inferences + 3 tools + 1 agent = 6
        assert_eq!(
            spans.len(),
            6,
            "expected 6 exported spans (2 inferences + 3 tools + 1 session)"
        );

        // All spans share the same trace_id.
        let trace_id = spans[0].span_context.trace_id();
        for s in &spans {
            assert_eq!(
                s.span_context.trace_id(),
                trace_id,
                "span '{}' should share trace_id",
                s.name
            );
        }

        // Agent span is root (no parent).
        let agent = spans
            .iter()
            .find(|s| s.name.starts_with("invoke_agent"))
            .expect("agent span not found");
        assert_eq!(agent.parent_span_id, opentelemetry::trace::SpanId::INVALID);

        // Both inference spans are children of the session.
        let inferences: Vec<_> = spans
            .iter()
            .filter(|s| s.name.starts_with("chat"))
            .collect();
        assert_eq!(inferences.len(), 2);
        for inf in &inferences {
            assert_eq!(
                inf.parent_span_id,
                agent.span_context.span_id(),
                "inference span should be child of agent"
            );
        }

        // Step 0 tools are children of the step-0 inference.
        let step0_inference = inferences
            .iter()
            .find(|s| {
                attr_map(s).get("awaken.step.index").map(|v| v.to_string()) == Some("0".to_string())
            })
            .expect("step 0 inference not found");
        let step0_tools: Vec<_> = spans
            .iter()
            .filter(|s| {
                let a = attr_map(s);
                s.name.starts_with("execute_tool")
                    && a.get("awaken.step.index").map(|v| v.to_string()) == Some("0".to_string())
            })
            .collect();
        assert_eq!(step0_tools.len(), 2, "expected 2 tools at step 0");
        for tool in &step0_tools {
            assert_eq!(
                tool.parent_span_id,
                step0_inference.span_context.span_id(),
                "step-0 tool should be child of step-0 inference"
            );
        }

        // Step 1 tool is child of step-1 inference.
        let step1_inference = inferences
            .iter()
            .find(|s| {
                attr_map(s).get("awaken.step.index").map(|v| v.to_string()) == Some("1".to_string())
            })
            .expect("step 1 inference not found");
        let step1_tools: Vec<_> = spans
            .iter()
            .filter(|s| {
                let a = attr_map(s);
                s.name.starts_with("execute_tool")
                    && a.get("awaken.step.index").map(|v| v.to_string()) == Some("1".to_string())
            })
            .collect();
        assert_eq!(step1_tools.len(), 1, "expected 1 tool at step 1");
        assert_eq!(
            step1_tools[0].parent_span_id,
            step1_inference.span_context.span_id(),
            "step-1 tool should be child of step-1 inference"
        );

        // All spans share the same awaken.run.id.
        for s in &spans {
            let attrs = attr_map(s);
            assert_eq!(
                attrs.get("awaken.run.id").map(|v| v.to_string()),
                Some("run-99".to_string()),
                "span '{}' missing awaken.run.id",
                s.name
            );
        }
    }

    #[test]
    fn tool_span_uses_absolute_timestamps_when_provided() {
        let (sink, exporter, provider) = make_capturing_sink();
        let context = SpanContext {
            run_id: "run-time".to_string(),
            thread_id: "thread-time".to_string(),
            agent_id: "agent-time".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        };
        sink.record(&MetricsEvent::Inference(GenAISpan {
            context: context.clone(),
            ..sample_genai_span()
        }));
        let started_at_ms: u64 = 1_700_000_000_000;
        let duration_ms: u64 = 150;
        let tool = ToolSpan {
            context: context.clone(),
            step_index: Some(0),
            duration_ms,
            started_at_ms,
            ended_at_ms: started_at_ms + duration_ms,
            ..sample_tool_span()
        };
        sink.record(&MetricsEvent::Tool(tool));
        sink.on_run_end(&AgentMetrics::default());
        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        let tool_span = spans
            .iter()
            .find(|s| s.name.starts_with("execute_tool"))
            .expect("tool span exported");
        let start_ms = tool_span
            .start_time
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let end_ms = tool_span
            .end_time
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert_eq!(
            start_ms, started_at_ms,
            "OTel start should equal started_at_ms"
        );
        assert_eq!(
            end_ms,
            started_at_ms + duration_ms,
            "OTel end should equal started_at_ms + duration"
        );
    }
}
