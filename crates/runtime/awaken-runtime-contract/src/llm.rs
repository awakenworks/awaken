//! Neutral model-inference port.
//!
//! `LlmExecutor` is the runtime's single seam to a model provider. The request
//! and response are provider-agnostic data: a provider adapter (for example
//! `awaken-provider-genai`) maps them to a concrete SDK. The runtime core never
//! names a provider SDK type, so G2/G10 hold and G22 stays enforceable — the
//! runtime validates the selected `ModelBinding` and never searches for another.

use async_trait::async_trait;
use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
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
    /// Multimodal content blocks (text, image). The adapter maps each block onto
    /// the provider's content representation.
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
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

/// One model response: the assistant turn plus optional usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub output: AssistantOutput,
    pub usage: Option<TokenUsage>,
}

/// One assistant turn as a list of content blocks. Text and tool requests may
/// interleave (`vec![Text, ToolUse, Text]`); a text-only turn is a natural end,
/// a turn with any `ToolUse` continues the loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantOutput {
    pub blocks: Vec<ContentBlock>,
}

impl AssistantOutput {
    /// A text-only turn.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            blocks: vec![ContentBlock::text(text)],
        }
    }

    /// A turn from explicit blocks (text and/or tool requests).
    pub fn from_blocks(blocks: Vec<ContentBlock>) -> Self {
        Self { blocks }
    }

    /// A turn from execution-side tool calls, each mapped to a `ToolUse` block.
    pub fn from_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            blocks: calls
                .into_iter()
                .map(|c| ContentBlock::tool_use(c.call_id, c.tool_id, c.arguments))
                .collect(),
        }
    }

    /// The turn's combined text across its `Text` blocks.
    pub fn text_content(&self) -> String {
        extract_text(&self.blocks)
    }

    /// The tool calls this turn requests, in order, projected onto the
    /// execution-side [`ToolCall`] (`ToolUse.id`/`name`/`input`).
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => Some(ToolCall {
                    call_id: id.clone(),
                    tool_id: name.clone(),
                    arguments: input.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Error)]
pub enum Error {
    /// The selected binding cannot be served (unknown model, bad endpoint).
    /// Permanent: retrying will not help.
    #[error("model binding rejected: {0}")]
    Binding(String),
    /// The provider call failed permanently (auth, quota exhausted, bad request,
    /// context overflow). Not worth retrying.
    #[error("model inference failed: {0}")]
    Inference(String),
    /// A transient failure worth retrying with backoff (rate limit, overload,
    /// 5xx, connection reset, timeout).
    #[error("model inference transiently failed: {0}")]
    Transient(String),
}

impl Error {
    /// Whether the runtime should retry the call after backoff.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Transient(_))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Where a provider pushes a turn's content as it streams. Best-effort live
/// progress only — never committed truth, which is the returned `ChatResponse`
/// (G10/G13). The runtime implements this to forward chunks to its live stream.
#[async_trait]
pub trait DeltaSink: Send + Sync {
    /// A chunk of assistant text as it arrives. Each chunk is a complete, valid
    /// UTF-8 fragment (the `&str` type guarantees it), and the concatenation of
    /// all chunks equals the final text. A provider that decodes a byte
    /// transport must buffer an incomplete multi-byte sequence and never split a
    /// code point across chunks; the runtime concatenates chunks verbatim and
    /// never indexes into a chunk by byte.
    async fn on_text(&self, chunk: &str);

    /// A tool call surfaced as the turn produced it. Best-effort live progress;
    /// the committed call is the one in the returned response, not this. The
    /// default does nothing, so a sink that only cares about text need not
    /// implement it.
    async fn on_tool_call(&self, _call_id: &str, _tool_id: &str, _arguments: &serde_json::Value) {}
}

/// Provider-adapter port: turn one neutral request into one neutral response.
#[async_trait]
pub trait LlmExecutor: Send + Sync {
    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse>;

    /// Stream the turn's text to `sink` as it arrives, returning the same
    /// assembled response `infer` would. The default is a faithful degenerate
    /// stream — it runs `infer` and pushes the whole text as one chunk — so a
    /// non-streaming provider needs no extra code; a streaming provider overrides
    /// this to push real chunks. The returned response is the committed truth;
    /// the pushed chunks are best-effort live output.
    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse> {
        let response = self.infer(request).await?;
        let text = response.output.text_content();
        if !text.is_empty() {
            sink.on_text(&text).await;
        }
        for call in response.output.tool_calls() {
            sink.on_tool_call(&call.call_id, &call.tool_id, &call.arguments)
                .await;
        }
        Ok(response)
    }
}
