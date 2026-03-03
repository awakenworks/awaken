use serde::{Deserialize, Serialize};

/// A single LLM inference span (OTel GenAI aligned).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenAISpan {
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
