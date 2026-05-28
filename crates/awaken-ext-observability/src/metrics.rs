use std::collections::HashMap;

use awaken_runtime::extensions::background::TaskStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::stats::{ModelStats, ToolStats};

pub(crate) const TOOL_PAYLOAD_TRUNCATED_MARKER: &str = "__awaken_payload_truncated";

pub(crate) fn is_tool_payload_truncated(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|obj| obj.get(TOOL_PAYLOAD_TRUNCATED_MARKER))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Execution context shared by all observability spans.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpanContext {
    /// Run identifier.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run_id: String,
    /// Conversation thread identifier.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thread_id: String,
    /// Agent identifier.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_id: String,
    /// Parent run id (for delegated sub-agent runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    /// Parent tool call id that caused this run/event, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,

    // ── Attribution fields (ADR-0030 D2) ───────────────────────────────
    /// Content-addressed id of the agent's effective system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    /// Content-addressed ids of tool descriptions advertised at this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_desc_ids: Vec<String>,
    /// Content-addressed ids of skills active at this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_ids: Vec<String>,
    /// Operator-supplied release alias (e.g. `agents.weather@stable`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_tag: Option<String>,

    // ── Experiment fields (populated by ADR-0031; reserved here) ───────
    /// Active experiment id, if the resolve pipeline routed through one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment_id: Option<String>,
    /// Variant name selected for this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant_name: Option<String>,
}

/// Unified event type for all observability events.
//
// `clippy::large_enum_variant` triggers because `Inference(GenAISpan)`
// grew with content-capture fields (response_content + response_tool_calls
// + request_messages). Boxing would force every match-arm consumer (sinks,
// otel exporter, prometheus, runtime_stats, persistent sink, eval CRUD)
// to unbox at every site — a churny change for a perf concern that does
// not show in profiles. MetricsEvent values are short-lived (recorded
// then dropped), so the 200-byte variant disparity is acceptable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum MetricsEvent {
    Inference(GenAISpan),
    Tool(ToolSpan),
    Suspension(SuspensionSpan),
    Handoff(HandoffSpan),
    Delegation(DelegationSpan),
    EvaluationResult(EvaluationResultEvent),
    BackgroundTask(BackgroundTaskSpan),
}

impl MetricsEvent {
    /// Identifier of the run this event belongs to. Reads through to the
    /// inner span's `SpanContext.run_id` — every variant carries one.
    /// Used by sink adapters that need to route events per-run (e.g.
    /// writing into a `TraceStore` keyed by run id).
    pub fn run_id(&self) -> &str {
        match self {
            Self::Inference(s) => &s.context.run_id,
            Self::Tool(s) => &s.context.run_id,
            Self::Suspension(s) => &s.context.run_id,
            Self::Handoff(s) => &s.context.run_id,
            Self::Delegation(s) => &s.context.run_id,
            Self::EvaluationResult(s) => &s.context.run_id,
            Self::BackgroundTask(s) => &s.context.run_id,
        }
    }
}

/// Opt-in capture policy for potentially sensitive tool call payloads.
///
/// Tool arguments and results can contain user data or secrets.  The default
/// keeps them out of telemetry; embedders must explicitly opt in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolIoCapture {
    #[default]
    Disabled,
    Arguments,
    Results,
    ArgumentsAndResults,
}

impl ToolIoCapture {
    pub fn captures_arguments(self) -> bool {
        matches!(self, Self::Arguments | Self::ArgumentsAndResults)
    }

    pub fn captures_results(self) -> bool {
        matches!(self, Self::Results | Self::ArgumentsAndResults)
    }
}

/// Opt-in capture policy for the assistant response payload on a
/// [`GenAISpan`] (`response_content` + `response_tool_calls`).
///
/// Assistant turns can carry user data or sensitive output. The default
/// keeps the payload out of telemetry; embedders that want to replay a
/// production trace as an eval fixture (ADR-0032 D5) must explicitly
/// opt in via `ObservabilityPlugin::with_content_capture(Enabled)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentCapture {
    #[default]
    Disabled,
    Enabled,
}

impl ContentCapture {
    pub fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// A single LLM inference span (OTel GenAI aligned).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenAISpan {
    /// Execution context (run, thread, agent).
    #[serde(flatten)]
    pub context: SpanContext,
    /// Which step in the run (0-based), incremented per inference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_index: Option<u32>,
    /// OTel: `gen_ai.request.model`.
    pub model: String,
    /// OTel: `gen_ai.provider.name`.
    pub provider: String,
    /// OTel: `gen_ai.operation.name`.
    pub operation: String,
    /// OTel: `gen_ai.response.model`.
    pub response_model: Option<String>,
    /// OTel: `gen_ai.response.id`.
    pub response_id: Option<String>,
    /// OTel: `gen_ai.response.finish_reasons`.
    pub finish_reasons: Vec<String>,
    /// OTel: `error.type`.
    pub error_type: Option<String>,
    /// Classified error category (e.g. `rate_limit`, `timeout`).
    pub error_class: Option<String>,
    /// OTel: `gen_ai.usage.reasoning.output_tokens`.
    pub thinking_tokens: Option<i32>,
    /// OTel: `gen_ai.usage.input_tokens`.
    pub input_tokens: Option<i32>,
    /// OTel: `gen_ai.usage.output_tokens`.
    pub output_tokens: Option<i32>,
    pub total_tokens: Option<i32>,
    /// OTel: `gen_ai.usage.cache_read.input_tokens`.
    pub cache_read_input_tokens: Option<i32>,
    /// OTel: `gen_ai.usage.cache_creation.input_tokens`.
    pub cache_creation_input_tokens: Option<i32>,
    /// OTel: `gen_ai.request.temperature`.
    pub temperature: Option<f64>,
    /// OTel: `gen_ai.request.top_p`.
    pub top_p: Option<f64>,
    /// OTel: `gen_ai.request.max_tokens`.
    pub max_tokens: Option<u32>,
    /// OTel: `gen_ai.request.stop_sequences`.
    pub stop_sequences: Vec<String>,
    /// Local duration used to set the exported span start/end timestamps.
    pub duration_ms: u64,
    /// Wall-clock start (epoch ms). Defaults to 0 for legacy payloads — OTel
    /// sinks that need a real start time fall back to `ended_at_ms - duration`.
    #[serde(default)]
    pub started_at_ms: u64,
    /// Wall-clock end (epoch ms). Defaults to 0 for legacy payloads.
    #[serde(default)]
    pub ended_at_ms: u64,
    /// Assistant text content blocks returned by the LLM, captured only when
    /// `ObservabilityPlugin::with_content_capture(Enabled)` is set on the
    /// plugin (default: disabled). Stored as opaque JSON so this crate
    /// stays decoupled from `awaken_runtime_contract::contract::content::ContentBlock`
    /// evolution; ADR-0032 D5 (`awaken-eval` trace→fixture converter)
    /// deserialises it back into the concrete contract type.
    ///
    /// `None` when capture is disabled, when the run errored before
    /// producing a response, or in legacy NDJSON written before this field
    /// existed (`#[serde(default)]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_content: Option<Value>,
    /// Tool calls emitted by the assistant turn (StopReason::ToolUse path).
    /// Same capture gating and `Value` shape as `response_content`; kept on
    /// a separate field so a chat-text turn and a tool-use turn each
    /// round-trip without ambiguity about which side carried the payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_tool_calls: Option<Value>,
    /// Request message history sent to the LLM on this turn (typically
    /// the user prompt + accumulated assistant/tool messages). Captured
    /// only when `ContentCapture::Enabled` and only on the *first*
    /// inference of a run — subsequent spans would carry growing copies
    /// of the same history, so the trade-off is "one fixed payload per
    /// run" instead of `O(turns²)` storage.
    ///
    /// Lets ADR-0032 D5's trace→fixture converter recover the original
    /// `user_input` without operator help. `None` when capture is
    /// disabled, when this is not the first inference, or in legacy
    /// NDJSON written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_messages: Option<Value>,
}

/// A single tool execution span (OTel GenAI aligned).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpan {
    /// Execution context (run, thread, agent).
    #[serde(flatten)]
    pub context: SpanContext,
    /// Step index matching the inference that triggered this tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_index: Option<u32>,
    /// OTel: `gen_ai.tool.name`.
    pub name: String,
    /// OTel: `gen_ai.operation.name`.
    pub operation: String,
    /// OTel: `gen_ai.tool.call.id`.
    pub call_id: String,
    /// OTel: `gen_ai.tool.type`.
    pub tool_type: String,
    /// OTel opt-in: `gen_ai.tool.call.arguments`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_arguments: Option<Value>,
    /// OTel opt-in: `gen_ai.tool.call.result`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_result: Option<Value>,
    /// OTel: `error.type`.
    pub error_type: Option<String>,
    pub duration_ms: u64,
    /// Wall-clock start (epoch ms). Defaults to 0 for legacy payloads.
    #[serde(default)]
    pub started_at_ms: u64,
    /// Wall-clock end (epoch ms). Defaults to 0 for legacy payloads.
    #[serde(default)]
    pub ended_at_ms: u64,
}

impl ToolSpan {
    pub fn is_success(&self) -> bool {
        self.error_type.is_none()
    }

    pub fn has_truncated_payload(&self) -> bool {
        self.call_arguments
            .as_ref()
            .is_some_and(is_tool_payload_truncated)
            || self
                .call_result
                .as_ref()
                .is_some_and(is_tool_payload_truncated)
    }
}

/// Result of evaluating a GenAI response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResultEvent {
    /// Execution context (run, thread, agent).
    #[serde(flatten)]
    pub context: SpanContext,
    /// OTel: `gen_ai.evaluation.name`.
    pub name: String,
    /// OTel: `gen_ai.evaluation.score.label`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_label: Option<String>,
    /// OTel: `gen_ai.evaluation.score.value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_value: Option<f64>,
    /// OTel: `gen_ai.evaluation.explanation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// OTel: `gen_ai.response.id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// OTel: `error.type`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    pub timestamp_ms: u64,
}

/// Span for tool suspension/resume events (HITL decisions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuspensionSpan {
    /// Execution context (run, thread, agent).
    #[serde(flatten)]
    pub context: SpanContext,
    pub tool_call_id: String,
    pub tool_name: String,
    /// "suspended" or "resumed"
    pub action: String,
    /// Resume mode if resumed (e.g., "use_decision", "replay", "pass_decision", "cancel")
    pub resume_mode: Option<String>,
    pub duration_ms: Option<u64>,
    pub timestamp_ms: u64,
}

/// Span for agent handoff events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffSpan {
    /// Execution context (run, thread, agent).
    #[serde(flatten)]
    pub context: SpanContext,
    pub from_agent_id: String,
    pub to_agent_id: String,
    pub reason: Option<String>,
    pub timestamp_ms: u64,
}

/// Span for A2A delegation events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationSpan {
    /// Execution context (run, thread, agent).
    #[serde(flatten)]
    pub context: SpanContext,
    pub parent_run_id: String,
    pub child_run_id: Option<String>,
    pub target_agent_id: String,
    pub tool_call_id: String,
    pub duration_ms: Option<u64>,
    pub success: bool,
    pub error_message: Option<String>,
    pub timestamp_ms: u64,
}

/// Lifecycle span for background task execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTaskSpan {
    /// Parent execution context (run, thread, agent).
    #[serde(flatten)]
    pub context: SpanContext,
    pub task_id: String,
    pub task_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_name: Option<String>,
    pub description: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
}

impl BackgroundTaskSpan {
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }
}

/// Aggregated metrics for an agent session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentMetrics {
    #[serde(default)]
    pub inferences: Vec<GenAISpan>,
    #[serde(default)]
    pub tools: Vec<ToolSpan>,
    #[serde(default)]
    pub evaluations: Vec<EvaluationResultEvent>,
    #[serde(default)]
    pub suspensions: Vec<SuspensionSpan>,
    #[serde(default)]
    pub handoffs: Vec<HandoffSpan>,
    #[serde(default)]
    pub delegations: Vec<DelegationSpan>,
    #[serde(default)]
    pub background_tasks: Vec<BackgroundTaskSpan>,
    #[serde(default)]
    pub session_duration_ms: u64,
}

impl AgentMetrics {
    pub fn total_input_tokens(&self) -> i32 {
        self.inferences.iter().filter_map(|s| s.input_tokens).sum()
    }

    pub fn total_output_tokens(&self) -> i32 {
        self.inferences.iter().filter_map(|s| s.output_tokens).sum()
    }

    pub fn total_tokens(&self) -> i32 {
        self.inferences.iter().filter_map(|s| s.total_tokens).sum()
    }

    pub fn total_cache_read_tokens(&self) -> i32 {
        self.inferences
            .iter()
            .filter_map(|s| s.cache_read_input_tokens)
            .sum()
    }

    pub fn total_cache_creation_tokens(&self) -> i32 {
        self.inferences
            .iter()
            .filter_map(|s| s.cache_creation_input_tokens)
            .sum()
    }

    pub fn total_inference_duration_ms(&self) -> u64 {
        self.inferences.iter().map(|s| s.duration_ms).sum()
    }

    pub fn total_tool_duration_ms(&self) -> u64 {
        self.tools.iter().map(|s| s.duration_ms).sum()
    }

    pub fn inference_count(&self) -> usize {
        self.inferences.len()
    }

    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    pub fn tool_failures(&self) -> usize {
        self.tools.iter().filter(|t| !t.is_success()).count()
    }

    pub fn total_suspensions(&self) -> usize {
        self.suspensions.len()
    }

    pub fn total_handoffs(&self) -> usize {
        self.handoffs.len()
    }

    pub fn total_delegations(&self) -> usize {
        self.delegations.len()
    }

    pub fn total_background_tasks(&self) -> usize {
        self.background_tasks.len()
    }

    pub fn successful_delegations(&self) -> usize {
        self.delegations.iter().filter(|d| d.success).count()
    }

    /// Inference statistics grouped by `(model, provider)`, sorted by model name.
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

    /// All events captured during the run, **grouped by type** (not by time).
    /// Order: inferences → tools → suspensions → handoffs → delegations →
    /// evaluations → background_tasks. Callers that need chronological order
    /// must sort by the per-event timestamp themselves.
    pub fn events(&self) -> Vec<MetricsEvent> {
        let mut events = Vec::with_capacity(
            self.inferences.len()
                + self.tools.len()
                + self.suspensions.len()
                + self.handoffs.len()
                + self.delegations.len()
                + self.evaluations.len()
                + self.background_tasks.len(),
        );
        events.extend(self.inferences.iter().cloned().map(MetricsEvent::Inference));
        events.extend(self.tools.iter().cloned().map(MetricsEvent::Tool));
        events.extend(
            self.suspensions
                .iter()
                .cloned()
                .map(MetricsEvent::Suspension),
        );
        events.extend(self.handoffs.iter().cloned().map(MetricsEvent::Handoff));
        events.extend(
            self.delegations
                .iter()
                .cloned()
                .map(MetricsEvent::Delegation),
        );
        events.extend(
            self.evaluations
                .iter()
                .cloned()
                .map(MetricsEvent::EvaluationResult),
        );
        events.extend(
            self.background_tasks
                .iter()
                .cloned()
                .map(MetricsEvent::BackgroundTask),
        );
        events
    }

    /// Tool execution statistics grouped by tool name, sorted by tool name.
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

    /// Tool execution statistics grouped by `(agent_id, tool)`.
    ///
    /// Result is sorted lexicographically by `(agent_id, tool)` so reports
    /// diff cleanly across runs.  Empty agent ids are preserved as their
    /// own bucket, which makes it obvious when a span landed without
    /// run-identity attribution.
    pub fn stats_by_agent_and_tool(&self) -> Vec<crate::stats::AgentToolStats> {
        use crate::stats::AgentToolStats;
        let mut map: HashMap<(String, String), AgentToolStats> = HashMap::new();
        for span in &self.tools {
            let key = (span.context.agent_id.clone(), span.name.clone());
            let entry = map.entry(key.clone()).or_insert_with(|| AgentToolStats {
                agent_id: key.0.clone(),
                tool: key.1.clone(),
                ..Default::default()
            });
            entry.call_count += 1;
            if !span.is_success() {
                entry.failure_count += 1;
            }
            entry.total_duration_ms += span.duration_ms;
        }
        let mut result: Vec<AgentToolStats> = map.into_values().collect();
        result.sort_by(|a, b| {
            a.agent_id
                .cmp(&b.agent_id)
                .then_with(|| a.tool.cmp(&b.tool))
        });
        result
    }
}

#[cfg(test)]
#[path = "metrics_attribution_test.rs"]
mod attribution_tests;

#[cfg(test)]
#[path = "metrics_test.rs"]
mod tests;
