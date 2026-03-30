use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::stats::{ModelStats, ToolStats};

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
}

/// Unified event type for all observability events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MetricsEvent {
    Inference(GenAISpan),
    Tool(ToolSpan),
    Suspension(SuspensionSpan),
    Handoff(HandoffSpan),
    Delegation(DelegationSpan),
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
    /// OTel: `gen_ai.usage.thinking_tokens`.
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
    /// OTel: `gen_ai.client.operation.duration`.
    pub duration_ms: u64,
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
    /// OTel: `error.type`.
    pub error_type: Option<String>,
    pub duration_ms: u64,
}

impl ToolSpan {
    pub fn is_success(&self) -> bool {
        self.error_type.is_none()
    }
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

/// Aggregated metrics for an agent session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentMetrics {
    pub inferences: Vec<GenAISpan>,
    pub tools: Vec<ToolSpan>,
    pub suspensions: Vec<SuspensionSpan>,
    pub handoffs: Vec<HandoffSpan>,
    pub delegations: Vec<DelegationSpan>,
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

    /// Get all events as a unified stream, ordered by type
    /// (inferences, tools, suspensions, handoffs, delegations).
    pub fn events(&self) -> Vec<MetricsEvent> {
        let mut events = Vec::with_capacity(
            self.inferences.len()
                + self.tools.len()
                + self.suspensions.len()
                + self.handoffs.len()
                + self.delegations.len(),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_genai_span(model: &str, input: Option<i32>, output: Option<i32>) -> GenAISpan {
        GenAISpan {
            context: SpanContext::default(),
            step_index: None,
            model: model.to_string(),
            provider: "test".to_string(),
            operation: "chat".to_string(),
            response_model: None,
            response_id: None,
            finish_reasons: Vec::new(),
            error_type: None,
            error_class: None,
            thinking_tokens: None,
            input_tokens: input,
            output_tokens: output,
            total_tokens: match (input, output) {
                (Some(i), Some(o)) => Some(i + o),
                _ => None,
            },
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: Vec::new(),
            duration_ms: 100,
        }
    }

    fn make_tool_span(name: &str, error: bool) -> ToolSpan {
        ToolSpan {
            context: SpanContext::default(),
            step_index: None,
            name: name.to_string(),
            operation: "execute_tool".to_string(),
            call_id: format!("call_{name}"),
            tool_type: "function".to_string(),
            error_type: if error {
                Some("tool_error".to_string())
            } else {
                None
            },
            duration_ms: 50,
        }
    }

    // ---- SpanContext serde roundtrip ----

    #[test]
    fn span_context_default_has_empty_fields() {
        let ctx = SpanContext::default();
        assert!(ctx.run_id.is_empty());
        assert!(ctx.thread_id.is_empty());
        assert!(ctx.agent_id.is_empty());
        assert!(ctx.parent_run_id.is_none());
    }

    #[test]
    fn span_context_serde_roundtrip() {
        let ctx = SpanContext {
            run_id: "run-1".to_string(),
            thread_id: "thread-1".to_string(),
            agent_id: "agent-1".to_string(),
            parent_run_id: Some("parent-run-1".to_string()),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let restored: SpanContext = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.run_id, "run-1");
        assert_eq!(restored.thread_id, "thread-1");
        assert_eq!(restored.agent_id, "agent-1");
        assert_eq!(restored.parent_run_id.as_deref(), Some("parent-run-1"));
    }

    #[test]
    fn span_context_serde_skips_empty_fields() {
        let ctx = SpanContext::default();
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(!json.contains("run_id"));
        assert!(!json.contains("thread_id"));
        assert!(!json.contains("agent_id"));
        assert!(!json.contains("parent_run_id"));
    }

    // ---- AgentMetrics::default() ----

    #[test]
    fn default_returns_zeros() {
        let m = AgentMetrics::default();
        assert!(m.inferences.is_empty());
        assert!(m.tools.is_empty());
        assert_eq!(m.session_duration_ms, 0);
        assert_eq!(m.total_input_tokens(), 0);
        assert_eq!(m.total_output_tokens(), 0);
        assert_eq!(m.total_tokens(), 0);
        assert_eq!(m.total_cache_read_tokens(), 0);
        assert_eq!(m.total_cache_creation_tokens(), 0);
        assert_eq!(m.total_inference_duration_ms(), 0);
        assert_eq!(m.total_tool_duration_ms(), 0);
        assert_eq!(m.inference_count(), 0);
        assert_eq!(m.tool_count(), 0);
        assert_eq!(m.tool_failures(), 0);
    }

    // ---- total_input_tokens() ----

    #[test]
    fn total_input_tokens_sums_across_spans() {
        let m = AgentMetrics {
            inferences: vec![
                make_genai_span("m", Some(100), Some(50)),
                make_genai_span("m", Some(200), Some(75)),
            ],
            ..Default::default()
        };
        assert_eq!(m.total_input_tokens(), 300);
    }

    #[test]
    fn total_input_tokens_skips_none() {
        let m = AgentMetrics {
            inferences: vec![
                make_genai_span("m", Some(100), Some(50)),
                make_genai_span("m", None, Some(75)),
            ],
            ..Default::default()
        };
        assert_eq!(m.total_input_tokens(), 100);
    }

    // ---- total_output_tokens() ----

    #[test]
    fn total_output_tokens_sums_correctly() {
        let m = AgentMetrics {
            inferences: vec![
                make_genai_span("m", Some(100), Some(50)),
                make_genai_span("m", Some(200), Some(75)),
            ],
            ..Default::default()
        };
        assert_eq!(m.total_output_tokens(), 125);
    }

    // ---- total_cache_read_tokens() ----

    #[test]
    fn total_cache_read_tokens_handles_none_values() {
        let m = AgentMetrics {
            inferences: vec![
                GenAISpan {
                    cache_read_input_tokens: Some(30),
                    ..make_genai_span("m", Some(10), Some(5))
                },
                GenAISpan {
                    cache_read_input_tokens: None,
                    ..make_genai_span("m", Some(10), Some(5))
                },
                GenAISpan {
                    cache_read_input_tokens: Some(20),
                    ..make_genai_span("m", Some(10), Some(5))
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.total_cache_read_tokens(), 50);
    }

    #[test]
    fn total_cache_read_tokens_all_none_returns_zero() {
        let m = AgentMetrics {
            inferences: vec![
                make_genai_span("m", Some(10), Some(5)),
                make_genai_span("m", Some(10), Some(5)),
            ],
            ..Default::default()
        };
        assert_eq!(m.total_cache_read_tokens(), 0);
    }

    // ---- total_cache_creation_tokens() ----

    #[test]
    fn total_cache_creation_tokens_sums() {
        let m = AgentMetrics {
            inferences: vec![
                GenAISpan {
                    cache_creation_input_tokens: Some(10),
                    ..make_genai_span("m", Some(10), Some(5))
                },
                GenAISpan {
                    cache_creation_input_tokens: Some(20),
                    ..make_genai_span("m", Some(10), Some(5))
                },
                GenAISpan {
                    cache_creation_input_tokens: None,
                    ..make_genai_span("m", Some(10), Some(5))
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.total_cache_creation_tokens(), 30);
    }

    // ---- stats_by_model() ----

    #[test]
    fn stats_by_model_groups_and_aggregates() {
        let m = AgentMetrics {
            inferences: vec![
                GenAISpan {
                    provider: "openai".into(),
                    cache_read_input_tokens: Some(5),
                    ..make_genai_span("gpt-4", Some(100), Some(50))
                },
                GenAISpan {
                    provider: "openai".into(),
                    cache_read_input_tokens: Some(15),
                    ..make_genai_span("gpt-4", Some(200), Some(75))
                },
                GenAISpan {
                    provider: "anthropic".into(),
                    ..make_genai_span("claude-3", Some(150), Some(60))
                },
            ],
            ..Default::default()
        };
        let stats = m.stats_by_model();
        assert_eq!(stats.len(), 2);

        // Sorted by model name: claude-3 first
        assert_eq!(stats[0].model, "claude-3");
        assert_eq!(stats[0].inference_count, 1);
        assert_eq!(stats[0].input_tokens, 150);

        assert_eq!(stats[1].model, "gpt-4");
        assert_eq!(stats[1].inference_count, 2);
        assert_eq!(stats[1].input_tokens, 300);
        assert_eq!(stats[1].output_tokens, 125);
        assert_eq!(stats[1].cache_read_input_tokens, 20);
        assert_eq!(stats[1].total_duration_ms, 200);
    }

    #[test]
    fn stats_by_model_empty_inferences() {
        let m = AgentMetrics::default();
        assert!(m.stats_by_model().is_empty());
    }

    // ---- stats_by_tool() ----

    #[test]
    fn stats_by_tool_groups_and_aggregates() {
        let m = AgentMetrics {
            tools: vec![
                make_tool_span("search", false),
                make_tool_span("search", false),
                make_tool_span("write", true),
            ],
            ..Default::default()
        };
        let stats = m.stats_by_tool();
        assert_eq!(stats.len(), 2);

        let search = stats.iter().find(|s| s.name == "search").unwrap();
        assert_eq!(search.call_count, 2);
        assert_eq!(search.failure_count, 0);
        assert_eq!(search.total_duration_ms, 100);

        let write = stats.iter().find(|s| s.name == "write").unwrap();
        assert_eq!(write.call_count, 1);
        assert_eq!(write.failure_count, 1);
    }

    #[test]
    fn stats_by_tool_empty_tools() {
        let m = AgentMetrics::default();
        assert!(m.stats_by_tool().is_empty());
    }

    // ---- tool_failures() ----

    #[test]
    fn tool_failures_counts_non_success() {
        let m = AgentMetrics {
            tools: vec![
                make_tool_span("a", false),
                make_tool_span("b", true),
                make_tool_span("c", true),
                make_tool_span("d", false),
            ],
            ..Default::default()
        };
        assert_eq!(m.tool_failures(), 2);
    }

    // ---- inference_count() and tool_count() ----

    #[test]
    fn inference_count_and_tool_count() {
        let m = AgentMetrics {
            inferences: vec![
                make_genai_span("a", Some(1), Some(1)),
                make_genai_span("b", Some(2), Some(2)),
                make_genai_span("c", Some(3), Some(3)),
            ],
            tools: vec![make_tool_span("t1", false), make_tool_span("t2", false)],
            ..Default::default()
        };
        assert_eq!(m.inference_count(), 3);
        assert_eq!(m.tool_count(), 2);
    }

    // ---- Edge cases ----

    #[test]
    fn empty_spans_edge_case() {
        let m = AgentMetrics::default();
        assert_eq!(m.total_input_tokens(), 0);
        assert_eq!(m.total_output_tokens(), 0);
        assert_eq!(m.inference_count(), 0);
        assert_eq!(m.tool_count(), 0);
        assert!(m.stats_by_model().is_empty());
        assert!(m.stats_by_tool().is_empty());
    }

    #[test]
    fn zero_duration_spans() {
        let m = AgentMetrics {
            inferences: vec![GenAISpan {
                duration_ms: 0,
                ..make_genai_span("m", Some(10), Some(5))
            }],
            tools: vec![ToolSpan {
                duration_ms: 0,
                ..make_tool_span("t", false)
            }],
            ..Default::default()
        };
        assert_eq!(m.total_inference_duration_ms(), 0);
        assert_eq!(m.total_tool_duration_ms(), 0);
    }

    #[test]
    fn all_none_token_fields() {
        let m = AgentMetrics {
            inferences: vec![make_genai_span("m", None, None)],
            ..Default::default()
        };
        assert_eq!(m.total_input_tokens(), 0);
        assert_eq!(m.total_output_tokens(), 0);
        assert_eq!(m.total_tokens(), 0);
    }

    // ---- ToolSpan::is_success ----

    #[test]
    fn tool_span_is_success_true() {
        let span = make_tool_span("search", false);
        assert!(span.is_success());
    }

    #[test]
    fn tool_span_is_success_false() {
        let span = make_tool_span("write", true);
        assert!(!span.is_success());
    }

    // ---- New span type serde roundtrips ----

    #[test]
    fn suspension_span_serde_roundtrip() {
        let span = SuspensionSpan {
            context: SpanContext::default(),
            tool_call_id: "c1".to_string(),
            tool_name: "search".to_string(),
            action: "suspended".to_string(),
            resume_mode: Some("use_decision".to_string()),
            duration_ms: Some(5000),
            timestamp_ms: 1000,
        };
        let json = serde_json::to_string(&span).unwrap();
        let restored: SuspensionSpan = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tool_call_id, "c1");
        assert_eq!(restored.action, "suspended");
        assert_eq!(restored.resume_mode.as_deref(), Some("use_decision"));
        assert_eq!(restored.duration_ms, Some(5000));
    }

    #[test]
    fn handoff_span_serde_roundtrip() {
        let span = HandoffSpan {
            context: SpanContext::default(),
            from_agent_id: "agent-a".to_string(),
            to_agent_id: "agent-b".to_string(),
            reason: Some("escalation".to_string()),
            timestamp_ms: 2000,
        };
        let json = serde_json::to_string(&span).unwrap();
        let restored: HandoffSpan = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.from_agent_id, "agent-a");
        assert_eq!(restored.to_agent_id, "agent-b");
        assert_eq!(restored.reason.as_deref(), Some("escalation"));
    }

    #[test]
    fn delegation_span_serde_roundtrip() {
        let span = DelegationSpan {
            context: SpanContext::default(),
            parent_run_id: "run-1".to_string(),
            child_run_id: Some("run-2".to_string()),
            target_agent_id: "worker".to_string(),
            tool_call_id: "c1".to_string(),
            duration_ms: Some(500),
            success: true,
            error_message: None,
            timestamp_ms: 3000,
        };
        let json = serde_json::to_string(&span).unwrap();
        let restored: DelegationSpan = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.parent_run_id, "run-1");
        assert_eq!(restored.child_run_id.as_deref(), Some("run-2"));
        assert!(restored.success);
        assert!(restored.error_message.is_none());
    }

    // ---- AgentMetrics new helpers ----

    #[test]
    fn agent_metrics_total_suspensions() {
        let m = AgentMetrics {
            suspensions: vec![
                SuspensionSpan {
                    context: SpanContext::default(),
                    tool_call_id: "c1".to_string(),
                    tool_name: "s".to_string(),
                    action: "suspended".to_string(),
                    resume_mode: None,
                    duration_ms: None,
                    timestamp_ms: 0,
                },
                SuspensionSpan {
                    context: SpanContext::default(),
                    tool_call_id: "c1".to_string(),
                    tool_name: "s".to_string(),
                    action: "resumed".to_string(),
                    resume_mode: Some("use_decision".to_string()),
                    duration_ms: Some(100),
                    timestamp_ms: 100,
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.total_suspensions(), 2);
    }

    #[test]
    fn agent_metrics_total_delegations() {
        let m = AgentMetrics {
            delegations: vec![
                DelegationSpan {
                    context: SpanContext::default(),
                    parent_run_id: "r1".to_string(),
                    child_run_id: None,
                    target_agent_id: "w1".to_string(),
                    tool_call_id: "c1".to_string(),
                    duration_ms: None,
                    success: true,
                    error_message: None,
                    timestamp_ms: 0,
                },
                DelegationSpan {
                    context: SpanContext::default(),
                    parent_run_id: "r1".to_string(),
                    child_run_id: None,
                    target_agent_id: "w2".to_string(),
                    tool_call_id: "c2".to_string(),
                    duration_ms: None,
                    success: false,
                    error_message: Some("timeout".to_string()),
                    timestamp_ms: 0,
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.total_delegations(), 2);
    }

    #[test]
    fn agent_metrics_successful_delegations() {
        let m = AgentMetrics {
            delegations: vec![
                DelegationSpan {
                    context: SpanContext::default(),
                    parent_run_id: "r1".to_string(),
                    child_run_id: None,
                    target_agent_id: "w1".to_string(),
                    tool_call_id: "c1".to_string(),
                    duration_ms: None,
                    success: true,
                    error_message: None,
                    timestamp_ms: 0,
                },
                DelegationSpan {
                    context: SpanContext::default(),
                    parent_run_id: "r1".to_string(),
                    child_run_id: None,
                    target_agent_id: "w2".to_string(),
                    tool_call_id: "c2".to_string(),
                    duration_ms: None,
                    success: false,
                    error_message: Some("timeout".to_string()),
                    timestamp_ms: 0,
                },
                DelegationSpan {
                    context: SpanContext::default(),
                    parent_run_id: "r1".to_string(),
                    child_run_id: Some("r3".to_string()),
                    target_agent_id: "w3".to_string(),
                    tool_call_id: "c3".to_string(),
                    duration_ms: Some(200),
                    success: true,
                    error_message: None,
                    timestamp_ms: 0,
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.successful_delegations(), 2);
    }

    #[test]
    fn agent_metrics_default_has_empty_new_fields() {
        let m = AgentMetrics::default();
        assert!(m.suspensions.is_empty());
        assert!(m.handoffs.is_empty());
        assert!(m.delegations.is_empty());
        assert_eq!(m.total_suspensions(), 0);
        assert_eq!(m.total_handoffs(), 0);
        assert_eq!(m.total_delegations(), 0);
        assert_eq!(m.successful_delegations(), 0);
    }
}
