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
