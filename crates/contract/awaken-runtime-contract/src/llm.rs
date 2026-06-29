//! Neutral model-inference port.
//!
//! `LlmExecutor` is the runtime's single seam to a model provider. The request
//! and response are provider-agnostic data: a provider adapter (for example
//! `awaken-provider-genai`) maps them to a concrete SDK. The runtime core never
//! names a provider SDK type, so G2/G10 hold and G22 stays enforceable — the
//! runtime validates the selected `ModelBinding` and never searches for another.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::resolved::ModelBinding;

/// One model invocation request. Pure data so it can be logged, replayed, and
/// snapshotted without holding a live provider handle (G3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// The binding selected before activation; the adapter routes on this and
    /// does not pick a different model (G22).
    pub model_binding: ModelBinding,
    pub messages: Vec<ChatMessage>,
    /// Model-visible tool schemas resolved for this run. Empty means no tools.
    pub tools: Vec<ToolSchema>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: ChatContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Neutral message content. A turn is either text, a set of requested tool
/// calls (assistant), or a tool result fed back to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChatContent {
    Text(String),
    ToolCalls(Vec<ToolCall>),
    ToolResult { call_id: String, content: String },
}

/// A model's request to invoke one tool. `tool_id` is the resolved descriptor
/// id; `call_id` correlates the later result. No permission grant is implied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub tool_id: String,
    pub arguments: serde_json::Value,
}

/// Model-visible tool descriptor projected for inference. Carries no executable
/// handle — execution authority lives behind `ToolExecutor` and the gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub id: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// One model response. Either a natural-end text turn or a tool-call turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub output: AssistantOutput,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AssistantOutput {
    Text(String),
    ToolCalls(Vec<ToolCall>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Error)]
pub enum Error {
    /// The selected binding cannot be served (unknown model, bad endpoint).
    #[error("model binding rejected: {0}")]
    Binding(String),
    /// The provider call failed (transport, auth, upstream error).
    #[error("model inference failed: {0}")]
    Inference(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Provider-adapter port: turn one neutral request into one neutral response.
#[async_trait]
pub trait LlmExecutor: Send + Sync {
    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse>;
}
