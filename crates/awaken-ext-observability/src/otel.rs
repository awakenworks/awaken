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
use crate::sink::{MetricsSink, SinkError};

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

    /// Compose a stable key for background-task lookups. The effective run
    /// key (parent run when present) is paired with `task_id`.
    ///
    /// Why no `owner_thread_id` in the key? The lazy lookup path (a child
    /// inference recording with an ambient `parent_task_id`) only has the
    /// child's `SpanContext`, whose `thread_id` differs from the parent
    /// task's owner thread. Including thread in the key would break that
    /// lookup. Cross-manager `bg_N` collision is prevented one layer up by
    /// the **1 `BackgroundTaskManager` per `StateStore`** invariant
    /// documented on `BackgroundTaskPlugin`, so within a single sink all
    /// `task_id`s are unique. The orthogonal plugin-level dedup
    /// (`background_task_statuses`) still carries `owner_thread_id` as
    /// defense in depth at that layer.
    fn task_context_key(run_key: &str, task_id: &str) -> String {
        format!("{run_key}\u{1f}{task_id}")
    }

    /// Convenience: build the task key from a span context + task id, using
    /// the same effective run key (`parent_run_id` when set) we use elsewhere
    /// for background-task lineage.
    fn background_task_key_from_context(ctx: &SpanContext, task_id: &str) -> String {
        Self::task_context_key(&Self::run_key_for_background_context(ctx), task_id)
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
        attrs.extend(otel_attributes_for_inference(span));
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
        attrs.extend(otel_attributes_for_tool(span));
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

    fn background_task_context(
        &self,
        ctx: &SpanContext,
        task_id: &str,
    ) -> Option<opentelemetry::Context> {
        let key = Self::background_task_key_from_context(ctx, task_id);
        if let Some(active) = self.current_background_tasks.lock().get(&key).cloned() {
            return Some(active.cx);
        }
        self.background_task_contexts.lock().get(&key)
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
        if let Some(cx) = self.background_task_context(ctx, task_id) {
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

        let key = Self::background_task_key_from_context(ctx, task_id);
        self.background_task_contexts
            .lock()
            .insert(key.clone(), cx.clone());
        self.current_background_tasks.lock().insert(
            key,
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
            && let Some(cx) = self.background_task_context(&span.context, parent_task_id)
        {
            return cx;
        }
        if let Some(parent_tool_call_id) = span
            .context
            .parent_tool_call_id
            .as_deref()
            .filter(|id| !id.is_empty())
        {
            // Honor `parent_run_id` when present so a sub-agent run whose own
            // run_id differs from the spawning run still attaches to the
            // parent run's tool span instead of a stranded one in its own run.
            let run_key = Self::run_key_for_background_context(&span.context);
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

        let key = Self::background_task_key_from_context(&span.context, &span.task_id);
        let active = { self.current_background_tasks.lock().get(&key).cloned() };
        if let Some(active) = active {
            active.cx.span().set_attributes(attrs);
            if span.error_message.is_some() {
                active.cx.span().set_status(Status::error(
                    span.error_message.clone().unwrap_or_default(),
                ));
            }
            if span.is_terminal() {
                self.current_background_tasks.lock().remove(&key);
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
            .insert(key.clone(), cx.clone());

        if span.is_terminal() {
            cx.span().end_with_timestamp(end_time);
            self.end_deferred_root_if_background_idle(&Self::run_key_for_background_context(
                &span.context,
            ));
        } else {
            self.current_background_tasks.lock().insert(
                key,
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

    /// Force-close any background-task spans, pending tool spans, and the
    /// deferred root for `run_key`, marking them with `close_reason`. Useful
    /// when a caller knows no more events will arrive (process shutdown,
    /// abandoned run) and wants the trace to surface the abandonment instead
    /// of holding spans open indefinitely.
    pub fn flush_run(&self, run_key: &str, close_reason: &'static str) {
        let stale: Vec<(String, ActiveBackgroundTask)> = self
            .current_background_tasks
            .lock()
            .iter()
            .filter(|(_, active)| active.run_key == run_key)
            .map(|(id, active)| (id.clone(), active.clone()))
            .collect();

        for (key, active) in stale {
            active.cx.span().set_attribute(KeyValue::new(
                "awaken.background_task.close_reason",
                close_reason,
            ));
            active.cx.span().set_status(Status::error(format!(
                "background task closed before terminal status: {close_reason}"
            )));
            active.cx.span().end();
            self.current_background_tasks.lock().remove(&key);
        }

        self.end_pending_tool_spans_for_run(run_key);
        if let Some(attrs) = self.deferred_root_ends.lock().remove(run_key) {
            self.end_root_context_with_attrs(run_key, attrs);
        } else if let Some(cx) = self.root_contexts.lock().remove(run_key) {
            cx.span()
                .set_attribute(KeyValue::new("awaken.root.close_reason", close_reason));
            cx.span().end();
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

    fn flush_run(&self, run_key: &str, close_reason: &'static str) -> Result<(), SinkError> {
        OtelMetricsSink::flush_run(self, run_key, close_reason);
        Ok(())
    }
}

impl Drop for OtelMetricsSink {
    fn drop(&mut self) {
        self.end_all_pending_tool_spans();
        self.end_all_current_inferences();
        for (_, active) in self.current_background_tasks.lock().drain() {
            active.cx.span().set_attribute(KeyValue::new(
                "awaken.background_task.close_reason",
                "shutdown",
            ));
            active.cx.span().end();
        }
        for (_, cx) in self.root_contexts.lock().drain() {
            cx.span()
                .set_attribute(KeyValue::new("awaken.root.close_reason", "shutdown"));
            cx.span().end();
        }
    }
}

// ADR-0030 D2 attribution attribute keys.  Single source of truth so the
// wire format lives in one place and a typo can't ship bad telemetry.
const ATTR_PROMPT_ID: &str = "awaken.prompt_id";
const ATTR_TOOL_DESC_IDS: &str = "awaken.tool_desc_ids";
const ATTR_SKILL_IDS: &str = "awaken.skill_ids";
const ATTR_RELEASE_TAG: &str = "awaken.release.tag";
const ATTR_EXPERIMENT_ID: &str = "awaken.experiment.id";
const ATTR_EXPERIMENT_VARIANT: &str = "awaken.experiment.variant";

/// Map an inference span's `SpanContext` to its ADR-0030 D2 OTel
/// attribution `KeyValue`s. Pure function — unit-testable without a
/// tracer; called by `genai_attributes` to extend the standard GenAI
/// attribute set with `awaken.*` fields.
pub(crate) fn otel_attributes_for_inference(span: &GenAISpan) -> Vec<KeyValue> {
    attribution_kv_pairs(&span.context)
}

/// Map a tool span's `SpanContext` to its ADR-0030 D2 OTel attribution
/// `KeyValue`s. Pure function — counterpart of
/// [`otel_attributes_for_inference`] for tool execution spans.
pub(crate) fn otel_attributes_for_tool(span: &ToolSpan) -> Vec<KeyValue> {
    attribution_kv_pairs(&span.context)
}

fn attribution_kv_pairs(ctx: &SpanContext) -> Vec<KeyValue> {
    let mut out: Vec<KeyValue> = Vec::new();
    if let Some(p) = &ctx.prompt_id {
        out.push(KeyValue::new(ATTR_PROMPT_ID, p.clone()));
    }
    if !ctx.tool_desc_ids.is_empty() {
        // Use the OTel string-array shape so backends (Phoenix, Jaeger,
        // OTLP exporters) can index per-id rather than parsing a
        // comma-joined string.
        out.push(KeyValue::new(
            ATTR_TOOL_DESC_IDS,
            string_array_value(&ctx.tool_desc_ids),
        ));
    }
    if !ctx.skill_ids.is_empty() {
        out.push(KeyValue::new(
            ATTR_SKILL_IDS,
            string_array_value(&ctx.skill_ids),
        ));
    }
    if let Some(t) = &ctx.release_tag {
        out.push(KeyValue::new(ATTR_RELEASE_TAG, t.clone()));
    }
    if let Some(e) = &ctx.experiment_id {
        out.push(KeyValue::new(ATTR_EXPERIMENT_ID, e.clone()));
    }
    if let Some(v) = &ctx.variant_name {
        out.push(KeyValue::new(ATTR_EXPERIMENT_VARIANT, v.clone()));
    }
    out
}

fn string_array_value(values: &[String]) -> Value {
    Value::Array(Array::String(
        values.iter().cloned().map(StringValue::from).collect(),
    ))
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
#[path = "otel_test.rs"]
mod tests;

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
#[path = "otel_attribution_test.rs"]
mod attribution_otel_tests;
