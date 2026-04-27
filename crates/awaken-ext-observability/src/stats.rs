use serde::{Deserialize, Serialize};

/// Per-model aggregated inference statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelStats {
    pub model: String,
    pub provider: String,
    pub inference_count: usize,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub total_tokens: i32,
    pub cache_read_input_tokens: i32,
    pub cache_creation_input_tokens: i32,
    pub total_duration_ms: u64,
}

/// Per-tool aggregated execution statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolStats {
    pub name: String,
    pub call_count: usize,
    pub failure_count: usize,
    pub total_duration_ms: u64,
}

/// Per-(agent, tool) aggregated execution statistics.
///
/// Fills the gap left by [`ToolStats`], which collapses calls across all
/// agents in a multi-agent run.  When a [`crate::ToolSpan`] carries a
/// non-empty `context.agent_id`, this aggregation gives operators a way to
/// attribute tool churn to a specific agent without rebuilding the spans
/// themselves.
///
/// `agent_id` is the value lifted directly from `ToolSpan.context.agent_id`;
/// empty strings are preserved so callers can distinguish "no agent
/// recorded" from a legitimate ID.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentToolStats {
    pub agent_id: String,
    pub tool: String,
    pub call_count: usize,
    pub failure_count: usize,
    pub total_duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ModelStats ----

    #[test]
    fn model_stats_default() {
        let s = ModelStats::default();
        assert!(s.model.is_empty());
        assert!(s.provider.is_empty());
        assert_eq!(s.inference_count, 0);
        assert_eq!(s.input_tokens, 0);
        assert_eq!(s.output_tokens, 0);
        assert_eq!(s.total_tokens, 0);
        assert_eq!(s.cache_read_input_tokens, 0);
        assert_eq!(s.cache_creation_input_tokens, 0);
        assert_eq!(s.total_duration_ms, 0);
    }

    #[test]
    fn model_stats_aggregation_with_multiple_entries() {
        let mut s = ModelStats {
            model: "gpt-4".into(),
            provider: "openai".into(),
            ..Default::default()
        };

        // Simulate aggregating two inference spans
        s.inference_count += 1;
        s.input_tokens += 100;
        s.output_tokens += 50;
        s.total_tokens += 150;
        s.cache_read_input_tokens += 20;
        s.total_duration_ms += 200;

        s.inference_count += 1;
        s.input_tokens += 200;
        s.output_tokens += 75;
        s.total_tokens += 275;
        s.cache_creation_input_tokens += 10;
        s.total_duration_ms += 300;

        assert_eq!(s.inference_count, 2);
        assert_eq!(s.input_tokens, 300);
        assert_eq!(s.output_tokens, 125);
        assert_eq!(s.total_tokens, 425);
        assert_eq!(s.cache_read_input_tokens, 20);
        assert_eq!(s.cache_creation_input_tokens, 10);
        assert_eq!(s.total_duration_ms, 500);
    }

    #[test]
    fn model_stats_serde_roundtrip() {
        let s = ModelStats {
            model: "claude-3".into(),
            provider: "anthropic".into(),
            inference_count: 5,
            input_tokens: 1000,
            output_tokens: 500,
            total_tokens: 1500,
            cache_read_input_tokens: 200,
            cache_creation_input_tokens: 100,
            total_duration_ms: 5000,
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: ModelStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.model, "claude-3");
        assert_eq!(parsed.inference_count, 5);
        assert_eq!(parsed.total_duration_ms, 5000);
    }

    // ---- ToolStats ----

    #[test]
    fn tool_stats_default() {
        let s = ToolStats::default();
        assert!(s.name.is_empty());
        assert_eq!(s.call_count, 0);
        assert_eq!(s.failure_count, 0);
        assert_eq!(s.total_duration_ms, 0);
    }

    #[test]
    fn tool_stats_aggregation_with_multiple_entries() {
        let mut s = ToolStats {
            name: "search".into(),
            ..Default::default()
        };

        // Simulate aggregating three tool spans (1 failure)
        s.call_count += 1;
        s.total_duration_ms += 50;

        s.call_count += 1;
        s.failure_count += 1;
        s.total_duration_ms += 100;

        s.call_count += 1;
        s.total_duration_ms += 25;

        assert_eq!(s.call_count, 3);
        assert_eq!(s.failure_count, 1);
        assert_eq!(s.total_duration_ms, 175);
    }

    #[test]
    fn tool_stats_serde_roundtrip() {
        let s = ToolStats {
            name: "write_file".into(),
            call_count: 10,
            failure_count: 2,
            total_duration_ms: 3000,
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: ToolStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "write_file");
        assert_eq!(parsed.call_count, 10);
        assert_eq!(parsed.failure_count, 2);
        assert_eq!(parsed.total_duration_ms, 3000);
    }

    // ---- AgentToolStats ----

    #[test]
    fn agent_tool_stats_default() {
        let s = AgentToolStats::default();
        assert!(s.agent_id.is_empty());
        assert!(s.tool.is_empty());
        assert_eq!(s.call_count, 0);
        assert_eq!(s.failure_count, 0);
        assert_eq!(s.total_duration_ms, 0);
    }

    #[test]
    fn agent_tool_stats_aggregation() {
        let mut s = AgentToolStats {
            agent_id: "planner".into(),
            tool: "search".into(),
            ..Default::default()
        };
        s.call_count += 1;
        s.total_duration_ms += 100;
        s.call_count += 1;
        s.failure_count += 1;
        s.total_duration_ms += 50;
        assert_eq!(s.call_count, 2);
        assert_eq!(s.failure_count, 1);
        assert_eq!(s.total_duration_ms, 150);
    }

    #[test]
    fn agent_tool_stats_serde_roundtrip() {
        let s = AgentToolStats {
            agent_id: "worker".into(),
            tool: "write_file".into(),
            call_count: 7,
            failure_count: 1,
            total_duration_ms: 250,
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: AgentToolStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn agent_tool_stats_serde_carries_all_fields() {
        let s = AgentToolStats {
            agent_id: "alpha".into(),
            tool: "calc".into(),
            call_count: 1,
            failure_count: 0,
            total_duration_ms: 10,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""agent_id":"alpha""#));
        assert!(json.contains(r#""tool":"calc""#));
        assert!(json.contains(r#""call_count":1"#));
        assert!(json.contains(r#""failure_count":0"#));
        assert!(json.contains(r#""total_duration_ms":10"#));
    }
}
