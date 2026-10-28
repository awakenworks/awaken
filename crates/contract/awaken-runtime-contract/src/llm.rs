// @formal entrypoint model_binding.constructor_and_wire_share_one_admission
// @formal entrypoint runtime.max_token_continuation_requires_every_guard_and_exhaustion_never_succeeds
//! Neutral model-inference port.
//!
//! `LlmExecutor` is the runtime's single seam to a model provider. The request
//! and response are provider-agnostic data: a provider adapter (for example
//! `awaken-provider-genai`) maps them to a concrete SDK. The runtime core never
//! names a provider SDK type, so G2/G10 hold and G22 stays enforceable — the
//! runtime validates the selected `ModelBinding` and never searches for another.

use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_agent_contract::agent::message::Role;
use serde::{Deserialize, Serialize};
use {async_trait::async_trait, std::time::Duration};

use awaken_agent_contract::audit::model_request::TokenUsage;
use thiserror::Error;

use crate::agent_bindings::InferenceOptions;
use crate::resolved::{ModelBinding, ToolDescriptor};

mod inference_budget;
pub use inference_budget::{ModelAttemptTiming, ModelInferenceBudget, ModelInferenceBudgetError};

/// One model invocation request. Pure data so it can be logged, replayed, and
/// snapshotted without holding a live provider handle (G3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// The binding selected before activation; the adapter routes on this and
    /// does not pick a different model (G22).
    pub model_binding: ModelBinding,
    /// Immutable call controls compiled from the same Agent revision as the
    /// binding. Provider adapters must apply or reject them, never ignore them.
    #[serde(default)]
    pub inference: InferenceOptions,
    pub messages: Vec<ChatMessage>,
    /// Model-visible tool descriptors resolved for this run — the same
    /// [`ToolDescriptor`] the resolved spec carries. Its content identity is
    /// derived only when resolution needs it and never enters provider mapping.
    /// Empty means no tools.
    pub tools: Vec<ToolDescriptor>,
}

impl ChatRequest {
    /// Project the already selected endpoint's wire model name without changing
    /// the original binding, provider identity, or backend. Adapters use this
    /// before acquiring a transport/grant; this selects no new publication.
    pub fn project_upstream_model(&self, model: impl Into<String>) -> Result<ModelBinding> {
        let (provider, _, backend) = self.model_binding.identity();
        ModelBinding::parse(provider, model, backend).map_err(|error| {
            Error::Binding(error.to_string()).with_execution_evidence(
                crate::model_operation::ModelExecutionEvidence::NotDispatched(
                    crate::model_operation::NoDispatchProof::request_projection(),
                ),
            )
        })
    }
}

/// Attempt-bound transformation of logical model content before provider I/O.
///
/// The runtime owns when this port runs (after request construction and model
/// selection, before retry/telemetry/provider dispatch); an infrastructure
/// adapter owns how a logical File reference is authorized and resolved. The
/// returned request must contain no unresolved provider-visible references.
#[async_trait]
pub trait ModelContentMaterializer: Send + Sync {
    fn process_bound(&self) -> bool {
        false
    }
    async fn materialize(&self, request: ChatRequest) -> Result<ChatRequest>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    /// Multimodal content blocks (text, image). The adapter maps each block onto
    /// the provider's content representation.
    pub content: Vec<ContentBlock>,
}

impl ChatMessage {
    /// Text projection of the same content blocks used by committed Messages.
    pub fn text_content(&self) -> String {
        extract_text(&self.content)
    }
}

use awaken_tool_contract::tool::ToolCall;

/// One model response: the assistant message plus optional usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub output: AssistantOutput,
    pub usage: Option<TokenUsage>,
    /// Why the response ended, when the provider reports it. `None` means the
    /// provider gave no reason; the runtime treats that as a natural end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
}

impl ChatResponse {
    /// The terminal receipt a returned response carries.
    ///
    /// [`LlmExecutor`] admits `Ok` only once an adapter has decoded the
    /// provider's complete response for that invocation; an interrupted
    /// exchange, a local timeout, an abandoned future or a transport error
    /// must be an `Err` with no evidence. Runtime reads the receipt here
    /// instead of each call site deciding for itself what a successful return
    /// proves, so the port's rule is stated exactly once.
    #[must_use]
    pub const fn terminal_receipt(&self) -> crate::model_operation::TerminalReceipt {
        crate::model_operation::TerminalReceipt::from_response_envelope(
            crate::model_operation::ResponseEnvelope::CompleteBody,
        )
    }
}

/// Provider-neutral reason a response stopped. `MaxTokens` is the one the loop
/// acts on: it marks a truncated response that may need continuation recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// The model finished naturally.
    NaturalEnd,
    /// The output hit the response token limit and was truncated.
    MaxTokens,
    /// The response stopped to invoke one or more tools.
    ToolUse,
    /// A configured stop sequence matched.
    StopSequence,
    /// A safety/content filter ended the response.
    ContentFilter,
}

impl StopReason {
    /// Whether this response may consume one more in-place continuation.
    ///
    /// This is the loop's single admission rule: only a `MaxTokens` response
    /// with continuable public text or private reasoning, no tool call, and
    /// remaining budget may be continued. Keeping the predicate beside the
    /// existing stop-reason type prevents provider adapters and runtime callers
    /// from inventing parallel interpretations of truncation.
    #[must_use]
    pub const fn admits_continuation(
        self,
        has_tool_calls: bool,
        has_continuable_output: bool,
        completed_continuations: usize,
        max_continuations: usize,
    ) -> bool {
        matches!(self, Self::MaxTokens)
            && !has_tool_calls
            && has_continuable_output
            && completed_continuations < max_continuations
    }
}

#[cfg(kani)]
mod stop_reason_verification {
    use super::*;

    /// @obligation runtime.max_token_continuation_requires_every_guard_and_exhaustion_never_succeeds
    #[kani::proof]
    fn max_token_continuation_requires_every_exact_guard() {
        let has_tool_calls: bool = kani::any();
        let has_continuable_output: bool = kani::any();
        let completed_continuations: usize = kani::any();
        let max_continuations: usize = kani::any();

        let expected = !has_tool_calls
            && has_continuable_output
            && completed_continuations < max_continuations;
        assert_eq!(
            StopReason::MaxTokens.admits_continuation(
                has_tool_calls,
                has_continuable_output,
                completed_continuations,
                max_continuations,
            ),
            expected,
        );
        assert!(!StopReason::NaturalEnd.admits_continuation(
            has_tool_calls,
            has_continuable_output,
            completed_continuations,
            max_continuations,
        ));
        assert!(!StopReason::ToolUse.admits_continuation(
            has_tool_calls,
            has_continuable_output,
            completed_continuations,
            max_continuations,
        ));
        assert!(!StopReason::StopSequence.admits_continuation(
            has_tool_calls,
            has_continuable_output,
            completed_continuations,
            max_continuations,
        ));
        assert!(!StopReason::ContentFilter.admits_continuation(
            has_tool_calls,
            has_continuable_output,
            completed_continuations,
            max_continuations,
        ));
    }
}

/// One assistant response as a list of content blocks. Text and tool requests may
/// interleave (`vec![Text, ToolUse, Text]`); a text-only response is a natural end,
/// a response with any `ToolUse` continues the Step loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantOutput {
    pub blocks: Vec<ContentBlock>,
}

impl AssistantOutput {
    /// A text-only response.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            blocks: vec![ContentBlock::text(text)],
        }
    }

    /// A response from explicit blocks (text and/or tool requests).
    pub fn from_blocks(blocks: Vec<ContentBlock>) -> Self {
        Self { blocks }
    }

    /// A response from execution-side tool calls, each mapped to a `ToolUse` block.
    pub fn from_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            blocks: calls
                .into_iter()
                .map(|c| ContentBlock::tool_use(c.call_id, c.tool_id, c.arguments))
                .collect(),
        }
    }

    /// The response's combined text across its `Text` blocks.
    pub fn text_content(&self) -> String {
        extract_text(&self.blocks)
    }

    /// Whether the provider returned private reasoning that can explain a
    /// `MaxTokens` truncation even when no public answer or tool call fit in the
    /// response budget. It is never answer text and is never an ordinary
    /// successful response by itself.
    #[must_use]
    pub fn has_reasoning(&self) -> bool {
        self.blocks.iter().any(
            |block| matches!(block, ContentBlock::Thinking { text, .. } if !text.trim().is_empty()),
        )
    }

    /// The tool calls this response requests, in order, projected onto the
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

/// Stable coordinates evaluated immediately before one logical model request.
/// Provider retries remain inside that request and therefore do not re-enter
/// this admission port; continuations and model-pool failover calls do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRequestAdmissionRequest {
    pub run_id: awaken_agent_contract::agent::run::Id,
    pub thread_id: awaken_agent_contract::agent::thread::Id,
}

/// Decision made by the owning application at the runtime inference
/// chokepoint. `Pause` preserves the same Run via its ordinary Awaiting ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRequestAdmission {
    Admit,
    Pause(awaken_agent_contract::agent::awaiting::PauseReason),
}

/// Neutral live authority checked before every logical model request. Durable
/// accounting remains owned outside Runtime; this port only asks that owner for
/// the current decision and never caches it. Errors carry their original grade;
/// Runtime adds admission origin and pre-Provider evidence at the call boundary.
#[async_trait]
pub trait ModelRequestGate: Send + Sync {
    /// No independent governed execution survives the returned admission.
    /// Accounting receipts remain owned by their application authority.
    fn process_bound(&self) -> bool {
        false
    }
    async fn admit_model_request(
        &self,
        request: ModelRequestAdmissionRequest,
    ) -> Result<ModelRequestAdmission>;
}

// Compatibility for existing SDK consumers. Internal value-only consumers
// depend on the defining Agent contract rather than the engine SPI.
pub use awaken_agent_contract::thread::usage::{
    THREAD_USAGE_STATE_KEY, ThreadUsage, ThreadUsageKey,
};

#[cfg(test)]
mod thread_usage_key_tests {
    use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope, StateKey, Store};

    use super::*;

    #[test]
    fn a_drifted_usage_cell_fails_closed_instead_of_resetting_the_tally() {
        // ADR-0055 / audit #96: the accumulated tally must NEVER be silently reset.
        // A persisted `__usage` value that no longer deserializes into `ThreadUsage`
        // (a shape drift) makes `load` return Err — not the `Default` (a zeroed
        // tally). Both callers (the step loop and `merge_thread_usage`) match this
        // Err and SKIP the record rather than overwrite the drifted cell, so the
        // committed total survives. `load_or_default` is the opt-in lenient read.
        let mut store = Store::new();
        store.apply(&Command::set(
            Scope::Thread,
            MergePolicy::Commutative,
            THREAD_USAGE_STATE_KEY,
            serde_json::json!("this is not a ThreadUsage object"),
        ));
        let read = ThreadUsageKey::load(&store);
        assert!(
            read.is_err(),
            "a drifted usage cell must fail closed, got {read:?}"
        );
        // The opt-in lenient path is the ONLY one that resets — named at its call site.
        assert_eq!(
            ThreadUsageKey::load_or_default(&store),
            ThreadUsage::default()
        );
    }

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
/// Retryable: `Provider`, `InvalidResponse`, `RateLimited`, `Overloaded`,
/// `Timeout`. Never retried: every permanent rejection (retrying an identical
/// request cannot succeed), a hard `UsageLimit`, and `Cancelled`.
#[derive(Debug, Error)]
pub enum Error {
    /// An application-owned model admission failure, before Provider inference.
    /// The nested grade controls bounded retry; this origin says nothing about
    /// physical completion and cannot grant execution re-entry.
    #[error("model admission failed: {source}")]
    Admission { source: Box<Error> },
    /// The adapter's independent execution evidence. Unannotated failures are
    /// Unknown, never implicitly safe because a Rust future returned Err.
    /// Classification/health remains the enclosed error; this is not another
    /// retry policy or a provider-specific recovery exception.
    #[error("{source}")]
    Observed {
        source: Box<Error>,
        execution: crate::model_operation::ModelExecutionEvidence,
    },
    /// The selected binding cannot be served (unknown model, bad endpoint).
    /// Permanent: retrying will not help.
    #[error("model binding rejected: {0}")]
    Binding(String),
    /// A generic provider-side failure (5xx, connection reset, unclassified
    /// transport error). Retryable with backoff.
    #[error("provider error: {0}")]
    Provider(String),
    /// The remote operation reached a terminal response, but that response
    /// cannot enter the neutral protocol. Retryable from the original request;
    /// unlike a transport interruption, streamed bytes from this attempt are
    /// not a confirmed prefix and must never steer the next attempt.
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    /// The provider rate-limited the request (429). Retryable; honors the
    /// server's `Retry-After` hint when present.
    #[error("rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<Duration>,
    },
    /// The provider is overloaded (529/503). Retryable with a longer backoff
    /// base than a generic provider error.
    #[error("provider overloaded: {message}")]
    Overloaded {
        message: String,
        retry_after: Option<Duration>,
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
        reset_after: Option<Duration>,
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
    /// The run was cancelled while this model call was being prepared,
    /// admitted or observed. Not a provider failure: never retried, never
    /// counted against provider health and never a reason to fail over.
    #[error("model call cancelled: {0}")]
    Cancelled(String),
}

impl Error {
    /// Attach evidence at the boundary that actually established it. Wrappers
    /// remain flat; transport decorators must preserve the inner evidence.
    #[must_use]
    pub fn with_execution_evidence(
        self,
        execution: crate::model_operation::ModelExecutionEvidence,
    ) -> Self {
        let source = match self {
            Self::Observed { source, .. } => source,
            other => Box::new(other),
        };
        Self::Observed { source, execution }
    }

    #[must_use]
    pub fn execution_evidence(&self) -> crate::model_operation::ModelExecutionEvidence {
        match self {
            Self::Observed { execution, .. } => *execution,
            Self::Admission { source } => source.execution_evidence(),
            _ => crate::model_operation::ModelExecutionEvidence::Unknown,
        }
    }

    /// The existing failure-grade payload; origin and physical evidence remain separate.
    #[must_use]
    pub fn classification(&self) -> &Self {
        match self {
            Self::Observed { source, .. } | Self::Admission { source } => source.classification(),
            other => other,
        }
    }

    /// Whether retrying the identical call can succeed at all — the
    /// projection [`FailureClass::is_retryable`](crate::resilience::FailureClass::is_retryable)
    /// of [`class`](Self::class). This is not permission to retry: only
    /// [`RetryPolicy::decide`](crate::resilience::RetryPolicy::decide), which
    /// also requires execution evidence and budget, grants that.
    pub fn is_retryable(&self) -> bool {
        self.class().is_retryable()
    }

    /// Whether a retry may continue from text/tool fragments streamed by the
    /// failed attempt. A terminal but invalid response owns no reusable partial;
    /// transport failures retain the existing interruption-recovery behavior.
    #[must_use]
    pub fn retry_preserves_attempt_output(&self) -> bool {
        !matches!(self.classification(), Error::InvalidResponse(_))
    }

    /// The server's `Retry-After` hint, when the provider sent one.
    pub fn retry_after(&self) -> Option<Duration> {
        self.class().retry_after()
    }

    /// The provider's reset window for a [`UsageLimit`](Self::UsageLimit), when one
    /// was sent. Purely informational — recognised and surfaced, never scheduled
    /// on. `None` for every other classification.
    pub fn reset_hint(&self) -> Option<Duration> {
        self.class().reset_after()
    }

    /// The stable snake_case code classifying this error. A run that cannot
    /// recover automatically records this code in its terminal failure, so
    /// hosts can categorize faults without parsing messages.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Observed { source, .. } => source.code(),
            Error::Admission { source } => {
                use crate::resilience::FailureClass;
                match source.class() {
                    FailureClass::Transient | FailureClass::Overloaded { .. } => {
                        "model_admission_unavailable"
                    }
                    FailureClass::RateLimited { .. } => "model_admission_rate_limited",
                    FailureClass::UsageLimit { .. } => "model_admission_usage_limit",
                    FailureClass::Permanent => "model_admission_rejected",
                    FailureClass::Cancelled => "cancelled",
                }
            }
            Error::Binding(_) => "binding_rejected",
            Error::Provider(_) | Error::InvalidResponse(_) => "provider_error",
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
            Error::Cancelled(_) => "cancelled",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Where a provider pushes a response's content as it streams. Best-effort live
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

    /// A live reasoning fragment. Consumers may expose only the existence of the
    /// reasoning stream; the committed response remains the sole owner of its
    /// content. The default preserves adapters that do not expose reasoning.
    async fn on_reasoning(&self, _chunk: &str) {}

    /// A live increment of a tool call's input — `args_delta` is the NEW fragment
    /// as the model streams it. The provider adapter de-accumulates its own
    /// cumulative snapshots, so this is already a suffix and a sink forwards it with
    /// no diffing. Best-effort live progress; the committed call (parsed input) is in
    /// the returned response, not this. The default does nothing.
    async fn on_tool_call_delta(&self, _call_id: &str, _tool_id: &str, _args_delta: &str) {}
}

/// Provider-adapter port: turn one neutral request into one neutral response.
/// Ok requires protocol completion for this invocation, not merely transport
/// EOF. Err carries independent execution evidence; an unannotated error is
/// Unknown. Adapters must not hide unconfirmed retries behind a later success.
#[async_trait]
pub trait LlmExecutor: Send + Sync {
    /// Implementation capability, not authored model metadata or a stop
    /// receipt. Unknown/custom adapters remain opaque. Wrappers may forward
    /// only the exact selected transport's request-dependent capability.
    fn effect_scope(&self, _request: &ChatRequest) -> crate::model_operation::ModelEffectScope {
        crate::model_operation::ModelEffectScope::Opaque
    }

    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse>;

    /// Stream the response's text to `sink` as it arrives, returning the same
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
        // Non-streaming default: emit each tool call's whole input as one delta, the
        // same shape a streaming provider produces incrementally — so downstream sees
        // one uniform `ToolCallDelta` regardless of provider streaming support.
        for call in response.output.tool_calls() {
            let args_delta = serde_json::to_string(&call.arguments).unwrap_or_default();
            sink.on_tool_call_delta(&call.call_id, &call.tool_id, &args_delta)
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
        tally.record("fast", u(1, 1)); // a second Step on the same model accumulates
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
    fn retry_classification_and_codes_are_the_authoritative_decision_table() {
        // The doc contract: retryable ⇔ Provider | RateLimited | Overloaded | Timeout;
        // everything else is permanent. `code()` is the stable per-variant fault class
        // a terminal failure records. Assert both axes together so a new variant or a
        // reclassification cannot silently drift.
        let retryable: [(Error, &str); 5] = [
            (Error::Provider("5xx".into()), "provider_error"),
            (
                Error::InvalidResponse("tool call was serialized as text".into()),
                "provider_error",
            ),
            (
                Error::RateLimited {
                    message: "429".into(),
                    retry_after: None,
                },
                "rate_limited",
            ),
            (
                Error::Overloaded {
                    message: "529".into(),
                    retry_after: None,
                },
                "overloaded",
            ),
            (Error::Timeout("slow".into()), "timeout"),
        ];
        for (e, code) in retryable {
            assert!(e.is_retryable(), "{e:?} must be retryable");
            assert_eq!(e.code(), code, "code for {e:?}");
        }

        let permanent: [(Error, &str); 8] = [
            (Error::Binding("bad".into()), "binding_rejected"),
            (
                Error::ContextOverflow("too long".into()),
                "context_overflow",
            ),
            (Error::InvalidRequest("400".into()), "invalid_request"),
            (Error::Unauthorized("401".into()), "unauthorized"),
            (Error::ModelNotFound("404".into()), "model_not_found"),
            (Error::ContentFiltered("blocked".into()), "content_filtered"),
            (Error::LoginRequired("expired".into()), "login_required"),
            (Error::Cancelled("run cancelled".into()), "cancelled"),
        ];
        for (e, code) in permanent {
            assert!(!e.is_retryable(), "{e:?} must be permanent");
            assert_eq!(e.code(), code, "code for {e:?}");
        }
        // UsageLimit is surfaced but never auto-retried (covered separately).
        assert!(
            !Error::UsageLimit {
                message: "q".into(),
                reset_after: None
            }
            .is_retryable()
        );
    }

    #[test]
    fn only_an_invalid_terminal_response_discards_its_attempt_output() {
        // Causes: C1 a terminal provider response is structurally unusable; C2
        // a transport/provider failure may have ended after a confirmed partial;
        // C3 execution evidence wraps either classification. Effects: E1 C1
        // retries the original request without the invalid bytes; E2 C2 retains
        // the established partial-continuation behavior; E3 wrapping does not
        // change either decision. Rules O1=C1=>E1, O2=C2=>E2,
        // O3=(C1|C2)+C3=>same decision. This is recovery input policy only;
        // execution evidence remains the independent re-entry gate.
        let invalid = Error::InvalidResponse("invalid".into());
        assert!(!invalid.retry_preserves_attempt_output(), "O1/E1");
        assert!(
            Error::Provider("drop".into()).retry_preserves_attempt_output(),
            "O2/E2"
        );
        assert!(
            Error::Timeout("drop".into()).retry_preserves_attempt_output(),
            "O2/E2"
        );
        assert!(
            !invalid
                .with_execution_evidence(
                    crate::model_operation::ModelExecutionEvidence::Terminated(
                        crate::model_operation::TerminalReceipt::from_wire_status(Some(
                            "completed"
                        ))
                        .unwrap()
                    )
                )
                .retry_preserves_attempt_output(),
            "O3/E3"
        );
    }

    #[test]
    fn retry_after_surfaces_only_the_transient_backoff_hint_not_the_reset_window() {
        // `retry_after` is the same-call backoff hint carried ONLY by RateLimited and
        // Overloaded; a hard UsageLimit's window is a `reset_hint`, never a retry_after
        // (an immediate retry cannot clear a hard limit), and every other error has none.
        let d = Duration::from_secs(7);
        assert_eq!(
            Error::RateLimited {
                message: "429".into(),
                retry_after: Some(d),
            }
            .retry_after(),
            Some(d)
        );
        assert_eq!(
            Error::Overloaded {
                message: "529".into(),
                retry_after: Some(d),
            }
            .retry_after(),
            Some(d)
        );
        // A quota reset window never masquerades as a retry backoff.
        assert_eq!(
            Error::UsageLimit {
                message: "weekly".into(),
                reset_after: Some(d),
            }
            .retry_after(),
            None
        );
        assert_eq!(Error::Provider("5xx".into()).retry_after(), None);
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

#[cfg(test)]
mod admission_tests;
