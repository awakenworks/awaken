use super::spans::{GenAISpan, ToolSpan};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

/// Aggregated metrics for an agent session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentMetrics {
    pub inferences: Vec<GenAISpan>,
    pub tools: Vec<ToolSpan>,
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
