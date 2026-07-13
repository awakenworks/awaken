//! Neutral model-inference port.
//!
//! `LlmExecutor` is the runtime's single seam to a model provider. The request
//! and response are provider-agnostic data: a provider adapter (for example
//! `awaken-provider-genai`) maps them to a concrete SDK. The runtime core never
//! names a provider SDK type, so G2/G10 hold and G22 stays enforceable — the
//! runtime validates the selected `ModelBinding` and never searches for another.

use async_trait::async_trait;
use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_agent_contract::agent::state::{MergePolicy, Scope, StateKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::resolved::{ModelBinding, ToolDescriptor};

/// One model invocation request. Pure data so it can be logged, replayed, and
/// snapshotted without holding a live provider handle (G3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// The binding selected before activation; the adapter routes on this and
    /// does not pick a different model (G22).
    pub model_binding: ModelBinding,
    pub messages: Vec<ChatMessage>,
    /// Model-visible tool descriptors resolved for this run — the same
    /// [`ToolDescriptor`] the resolved spec carries (its `content_hash` rides
    /// along, ignored by the provider mapping). Empty means no tools.
    pub tools: Vec<ToolDescriptor>,
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

/// One model response: the assistant turn plus optional usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub output: AssistantOutput,
    pub usage: Option<TokenUsage>,
    /// Why the turn ended, when the provider reports it. `None` means the
    /// provider gave no reason; the runtime treats that as a natural end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
}

/// Provider-neutral reason a turn stopped. `MaxTokens` is the one the loop
/// acts on: it marks a truncated turn that may need continuation recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// The model finished naturally.
    EndTurn,
    /// The output hit the response token limit and was truncated.
    MaxTokens,
    /// The turn stopped to invoke one or more tools.
    ToolUse,
    /// A configured stop sequence matched.
    StopSequence,
    /// A safety/content filter ended the turn.
    ContentFilter,
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

/// One model invocation's token usage. `cache_read`/`cache_creation` are the
/// prompt-cache breakdown a provider may report (Anthropic `cache_read_input_tokens`
/// / `cache_creation_input_tokens`); `0` when the provider reports none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
}

impl TokenUsage {
    /// Field-wise saturating sum, used to accumulate usage across steps/turns.
    #[must_use]
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            prompt_tokens: self.prompt_tokens.saturating_add(other.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_add(other.completion_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_add(other.cache_read_tokens),
            cache_creation_tokens: self
                .cache_creation_tokens
                .saturating_add(other.cache_creation_tokens),
        }
    }
}

/// A thread's accumulated token usage **attributed per model** (a session may span
/// several models — per-turn overrides, sub-agents, native vs an external runtime).
/// The neutral truth the runtime records; adapters project the [`total`](Self::total)
/// (or the per-model breakdown) onto their wire. Stored under [`THREAD_USAGE_STATE_KEY`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadUsage {
    /// `model_ref -> usage on that model`.
    pub by_model: std::collections::BTreeMap<String, TokenUsage>,
}

impl ThreadUsage {
    /// Add one step's usage to `model`'s running total.
    pub fn record(&mut self, model: &str, usage: TokenUsage) {
        let entry = self.by_model.entry(model.to_string()).or_default();
        *entry = entry.saturating_add(usage);
    }

    /// Fold another tally into this one, per model — the seam that rolls a
    /// sub-agent's usage (recorded on its own isolated thread) into the parent
    /// thread's running total, so a delegated turn's tokens are not lost. Each
    /// model's counts accumulate independently (a sub-agent may run a different
    /// model than its parent).
    pub fn merge(&mut self, other: &ThreadUsage) {
        for (model, usage) in &other.by_model {
            self.record(model, *usage);
        }
    }

    /// True when no usage has been recorded (every sub-run over a deterministic
    /// model reports none — nothing to roll up).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_model.is_empty()
    }

    /// The session-level total across every model (what a session-usage field reports).
    #[must_use]
    pub fn total(&self) -> TokenUsage {
        self.by_model
            .values()
            .copied()
            .fold(TokenUsage::default(), TokenUsage::saturating_add)
    }
}

/// The thread-scoped committed-state key under which the run loop accumulates a
/// thread's [`ThreadUsage`] (serialized). Committed truth, so an adapter surfaces a
/// session's usage by reading thread state — the runtime records the fact without
/// naming any wire, and it survives a restart. Internal (the `__` prefix keeps it out
/// of any agent-authored state namespace).
pub const THREAD_USAGE_STATE_KEY: &str = "__usage";

/// Typed view over the thread-usage cell (ADR-0055). A read fails closed on a
/// shape drift (so a persisted run's accumulated tally is never silently reset),
/// and a write is `Commutative` so a step's tally shallow-merges the committed
/// object. Write-only: the loop reads the tally, folds a step's usage via
/// [`ThreadUsage::record`], and writes the whole value back.
pub struct ThreadUsageKey;

impl StateKey for ThreadUsageKey {
    const KEY: &'static str = THREAD_USAGE_STATE_KEY;
    const SCOPE: Scope = Scope::Thread;
    const MERGE: MergePolicy = MergePolicy::Commutative;
    type Value = ThreadUsage;
}

#[cfg(test)]
mod thread_usage_key_tests {
    use awaken_agent_contract::agent::state::{StateKey, Store};

    use super::*;

    #[test]
    fn record_then_write_reads_back_typed() {
        let mut store = Store::new();
        let usage = TokenUsage {
            prompt_tokens: 3,
            completion_tokens: 5,
            ..Default::default()
        };
        // The loop's discipline: read the tally (fail-closed), fold a step's usage,
        // write the whole value back.
        let mut tally = ThreadUsageKey::load(&store).unwrap();
        tally.record("m", usage);
        store.apply(&ThreadUsageKey::write(&tally));
        let read = ThreadUsageKey::load(&store).unwrap();
        assert_eq!(read.by_model["m"].prompt_tokens, 3);
        assert_eq!(read.by_model["m"].completion_tokens, 5);
    }
}

/// A classified inference failure. The variant is the classification: it
/// decides both the retry policy (`is_retryable`) and the stable code
/// (`code`) a run that cannot recover reports in its terminal failure.
/// Retryable: `Provider`, `RateLimited`, `Overloaded`, `Timeout`. Permanent:
/// everything else — retrying an identical request cannot succeed.
#[derive(Debug, Error)]
pub enum Error {
    /// The selected binding cannot be served (unknown model, bad endpoint).
    /// Permanent: retrying will not help.
    #[error("model binding rejected: {0}")]
    Binding(String),
    /// A generic provider-side failure (5xx, connection reset, unclassified
    /// transport error). Retryable with backoff.
    #[error("provider error: {0}")]
    Provider(String),
    /// The provider rate-limited the request (429). Retryable; honors the
    /// server's `Retry-After` hint when present.
    #[error("rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    /// The provider is overloaded (529/503). Retryable with a longer backoff
    /// base than a generic provider error.
    #[error("provider overloaded: {message}")]
    Overloaded {
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    /// The call or stream timed out (408/504, client-side timeout). Retryable.
    #[error("model call timed out: {0}")]
    Timeout(String),
    /// The prompt exceeds the model's context window (413, or 400 with a
    /// context-length message). Permanent for this request shape — only a
    /// smaller prompt can succeed.
    #[error("context overflow: {0}")]
    ContextOverflow(String),
    /// The request is malformed (400/422). Permanent.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Authentication or authorization failed (401/403). Permanent.
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    /// A HARD usage/quota exhaustion (a weekly/monthly/credit limit), distinct
    /// from a transient 429 [`RateLimited`](Self::RateLimited). Recognised and
    /// surfaced with a stable code so an operator/dashboard sees it — but NOT
    /// auto-retried (an immediate retry cannot clear a hard limit) and NOT
    /// auto-acted on (no credential cooldown / re-dispatch here; that is a
    /// control-plane decision). `reset_after` is the provider's reset window when
    /// sent — purely informational, nothing is scheduled from it.
    #[error("usage limit exhausted: {message}")]
    UsageLimit {
        message: String,
        reset_after: Option<std::time::Duration>,
    },
    /// The credential needs (re-)authentication — an expired grant / login
    /// required — distinct from a generic [`Unauthorized`](Self::Unauthorized).
    /// Recognised and surfaced; not auto-retried and not auto-refreshed here.
    #[error("login required: {0}")]
    LoginRequired(String),
    /// The requested model does not exist (404). Permanent.
    #[error("model not found: {0}")]
    ModelNotFound(String),
    /// A content-safety filter rejected the request or response. Permanent.
    #[error("content filtered: {0}")]
    ContentFiltered(String),
}

impl Error {
    /// Whether the runtime should retry the call after backoff.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Error::Provider(_)
                | Error::RateLimited { .. }
                | Error::Overloaded { .. }
                | Error::Timeout(_)
        )
    }

    /// The server's `Retry-After` hint, when the provider sent one.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Error::RateLimited { retry_after, .. } | Error::Overloaded { retry_after, .. } => {
                *retry_after
            }
            _ => None,
        }
    }

    /// The provider's reset window for a [`UsageLimit`](Self::UsageLimit), when one
    /// was sent. Purely informational — recognised and surfaced, never scheduled
    /// on. `None` for every other classification.
    pub fn reset_hint(&self) -> Option<std::time::Duration> {
        match self {
            Error::UsageLimit { reset_after, .. } => *reset_after,
            _ => None,
        }
    }

    /// The stable snake_case code classifying this error. A run that cannot
    /// recover automatically records this code in its terminal failure, so
    /// hosts can categorize faults without parsing messages.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Binding(_) => "binding_rejected",
            Error::Provider(_) => "provider_error",
            Error::RateLimited { .. } => "rate_limited",
            Error::Overloaded { .. } => "overloaded",
            Error::Timeout(_) => "timeout",
            Error::ContextOverflow(_) => "context_overflow",
            Error::InvalidRequest(_) => "invalid_request",
            Error::Unauthorized(_) => "unauthorized",
            Error::UsageLimit { .. } => "usage_limit",
            Error::LoginRequired(_) => "login_required",
            Error::ModelNotFound(_) => "model_not_found",
            Error::ContentFiltered(_) => "content_filtered",
        }
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

#[cfg(test)]
mod usage_tests {
    use super::{ThreadUsage, TokenUsage};

    fn u(p: u64, c: u64) -> TokenUsage {
        TokenUsage {
            prompt_tokens: p,
            completion_tokens: c,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        }
    }

    #[test]
    fn saturating_add_is_field_wise() {
        let a = TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 2,
            cache_read_tokens: 3,
            cache_creation_tokens: 4,
        };
        let b = TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 20,
            cache_read_tokens: 30,
            cache_creation_tokens: 40,
        };
        assert_eq!(
            a.saturating_add(b),
            TokenUsage {
                prompt_tokens: 11,
                completion_tokens: 22,
                cache_read_tokens: 33,
                cache_creation_tokens: 44
            }
        );
    }

    #[test]
    fn thread_usage_attributes_per_model_and_totals() {
        let mut tally = ThreadUsage::default();
        tally.record("fast", u(10, 5));
        tally.record("slow", u(100, 50));
        tally.record("fast", u(1, 1)); // a second turn on the same model accumulates
        assert_eq!(tally.by_model.get("fast").copied(), Some(u(11, 6)));
        assert_eq!(tally.by_model.get("slow").copied(), Some(u(100, 50)));
        // The session-level total sums across every model.
        assert_eq!(tally.total(), u(111, 56));
    }

    #[test]
    fn total_of_empty_is_zero() {
        assert_eq!(ThreadUsage::default().total(), TokenUsage::default());
    }
}

#[cfg(test)]
mod error_classification_tests {
    use super::Error;
    use std::time::Duration;

    #[test]
    fn usage_limit_is_surfaced_with_a_reset_hint_but_not_retryable() {
        let e = Error::UsageLimit {
            message: "quota exhausted".to_string(),
            reset_after: Some(Duration::from_secs(3600)),
        };
        assert_eq!(e.code(), "usage_limit");
        // Recognised and surfaced — never auto-retried and never auto-acted on.
        assert!(!e.is_retryable());
        // The reset window is informational only; it is not a retry backoff.
        assert_eq!(e.reset_hint(), Some(Duration::from_secs(3600)));
        assert_eq!(e.retry_after(), None);
    }

    #[test]
    fn login_required_is_permanent_and_distinct_from_unauthorized() {
        let lr = Error::LoginRequired("grant expired".to_string());
        assert_eq!(lr.code(), "login_required");
        assert!(!lr.is_retryable());
        assert_eq!(lr.reset_hint(), None);

        // A generic authorization failure keeps its own distinct code.
        assert_eq!(
            Error::Unauthorized("bad key".to_string()).code(),
            "unauthorized"
        );
    }
}
