//! Agent loop implementation with Phase-based plugin execution.
//!
//! The agent loop orchestrates the conversation between user, LLM, and tools:
//!
//! ```text
//! User Input → LLM → Tool Calls? → Execute Tools → LLM → ... → Final Response
//! ```
//!
//! # Phase Execution
//!
//! Each phase dispatches to its typed plugin hook:
//!
//! ```text
//! RunStart (once)
//!     │
//!     ▼
//! ┌─────────────────────────┐
//! │      StepStart          │ ← plugins can apply state patches
//! ├─────────────────────────┤
//! │    BeforeInference      │ ← plugins can inject prompt context, filter tools
//! ├─────────────────────────┤
//! │      [LLM CALL]         │
//! ├─────────────────────────┤
//! │    AfterInference       │
//! ├─────────────────────────┤
//! │  ┌───────────────────┐  │
//! │  │ BeforeToolExecute │  │ ← plugins can block/pending
//! │  ├───────────────────┤  │
//! │  │   [TOOL EXEC]     │  │
//! │  ├───────────────────┤  │
//! │  │ AfterToolExecute  │  │ ← plugins can add reminders
//! │  └───────────────────┘  │
//! ├─────────────────────────┤
//! │       StepEnd           │
//! └─────────────────────────┘
//!     │
//!     ▼
//! RunEnd (once)
//! ```

mod config;
mod core;
mod event_envelope_meta;
mod outcome;
mod parallel_state_merge;
mod plugin_runtime;
mod run_state;
mod state_commit;
mod stream_core;
mod stream_runner;
mod tool_exec;
mod truncation_recovery;

use crate::contracts::io::ResumeDecisionAction;
use crate::contracts::runtime::phase::Phase;
use crate::contracts::runtime::state::{reduce_state_actions, AnyStateAction, ScopeContext};
use crate::contracts::runtime::tool_call::{Tool, ToolResult};
use crate::contracts::runtime::ActivityManager;
use crate::contracts::runtime::{
    DecisionReplayPolicy, RunIdentity, RunLifecycleAction, RunLifecycleState, StreamResult,
    SuspendedCall, ToolCallResume, ToolCallResumeMode, ToolCallStatus, ToolExecutionRequest,
    ToolExecutionResult,
};
use crate::contracts::thread::CheckpointReason;
use crate::contracts::thread::{gen_message_id, Message, MessageMetadata, ToolCall};
use crate::contracts::RunContext;
use crate::contracts::{AgentEvent, RunAction, TerminationReason, ToolCallDecision};
use crate::engine::convert::{assistant_message, assistant_tool_calls, tool_response};
use crate::runtime::activity::ActivityHub;

use crate::runtime::loop_runner::state_commit::RunTokenTotals;
use crate::runtime::streaming::StreamCollector;
use async_stream::stream;
use futures::{Stream, StreamExt};
use genai::Client;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub use crate::contracts::runtime::ToolExecutor;
pub use crate::runtime::run_context::{
    await_or_cancel, is_cancelled, CancelAware, RunCancellationToken, StateCommitError,
    StateCommitter,
};
use config::StaticStepToolProvider;
pub use config::{Agent, BaseAgent, GenaiLlmExecutor, LlmRetryPolicy};
pub use config::{LlmEventStream, LlmExecutor};
pub use config::{StepToolInput, StepToolProvider, StepToolSnapshot};
#[cfg(test)]
use core::build_messages;
use core::{
    build_request_for_filtered_tools, inference_inputs_from_step, suspended_calls_from_ctx,
    tool_call_states_from_ctx, transition_tool_call_state, upsert_tool_call_state,
    ToolCallStateSeed, ToolCallStateTransition,
};
pub use outcome::{tool_map, tool_map_from_arc, AgentLoopError};
pub use outcome::{LoopOutcome, LoopStats, LoopUsage};
#[cfg(test)]
use plugin_runtime::emit_agent_phase;
#[cfg(test)]
use plugin_runtime::emit_cleanup_phases;
use run_state::LoopRunState;
pub use state_commit::ChannelStateCommitter;
use state_commit::PendingDeltaCommitContext;
use tirea_state::TrackedPatch;
#[cfg(test)]
use tokio_util::sync::CancellationToken;
#[cfg(test)]
use tool_exec::execute_single_tool_with_phases;
#[cfg(test)]
use tool_exec::execute_tools_parallel_with_phases;
pub use tool_exec::ExecuteToolsOutcome;
use tool_exec::{
    apply_tool_results_impl, apply_tool_results_to_session, caller_context_for_tool_execution,
    execute_single_tool_with_phases_deferred, step_metadata, ToolPhaseContext,
};
pub use tool_exec::{
    execute_tools, execute_tools_with_behaviors, execute_tools_with_config,
    ParallelToolExecutionMode, ParallelToolExecutor, SequentialToolExecutor,
};

/// Fully resolved agent wiring ready for execution.
///
/// Contains everything needed to run an agent loop: the agent,
/// the resolved tool map, and the run policy. This is a pure data struct
/// that can be inspected, mutated, and tested independently.
pub struct ResolvedRun {
    /// The agent (model, behavior, execution strategies, ...).
    ///
    /// Exposed as a concrete [`BaseAgent`] so callers can mutate fields
    /// (model, plugins, tool_executor, ...) between resolution and execution.
    /// Converted to `Arc<dyn Agent>` at the execution boundary.
    pub agent: BaseAgent,
    /// Resolved tool map after filtering and wiring.
    pub tools: HashMap<String, Arc<dyn Tool>>,
    /// Typed per-run policy.
    pub run_policy: crate::contracts::RunPolicy,
    /// Optional lineage seed for nested tool-driven runs.
    pub parent_tool_call_id: Option<String>,
}

impl ResolvedRun {
    /// Add or replace a tool in the resolved tool map.
    #[must_use]
    pub fn with_tool(mut self, id: String, tool: Arc<dyn Tool>) -> Self {
        self.tools.insert(id, tool);
        self
    }

    /// Overlay tools from a map (insert-if-absent semantics).
    pub fn overlay_tools(&mut self, tools: HashMap<String, Arc<dyn Tool>>) {
        for (id, tool) in tools {
            self.tools.entry(id).or_insert(tool);
        }
    }
}

pub(crate) fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

fn ensure_run_identity(
    agent: &dyn Agent,
    run_ctx: &mut RunContext,
    mut run_identity: RunIdentity,
) -> RunIdentity {
    if run_identity.run_id_opt().is_none() {
        run_identity.run_id = Uuid::now_v7().to_string();
    }
    if run_identity.agent_id_opt().is_none() {
        run_identity.agent_id = agent.id().to_string();
    }
    if run_identity.thread_id_opt().is_none() {
        run_identity.thread_id = run_ctx.thread_id().to_string();
    }
    run_ctx.set_run_identity(run_identity.clone());
    run_identity
}

#[cfg(test)]
pub(super) fn sync_run_lifecycle_for_termination(
    run_ctx: &mut RunContext,
    run_identity: &RunIdentity,
    termination: &TerminationReason,
) -> Result<(), AgentLoopError> {
    sync_run_lifecycle_for_termination_with_context(run_ctx, run_identity, termination)
}

fn sync_run_lifecycle_for_termination_with_context(
    run_ctx: &mut RunContext,
    run_identity: &RunIdentity,
    termination: &TerminationReason,
) -> Result<(), AgentLoopError> {
    if run_identity.run_id.trim().is_empty() {
        return Ok(());
    };

    let (status, done_reason) = termination.to_run_status();

    let base_state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let actions = vec![AnyStateAction::new::<RunLifecycleState>(
        RunLifecycleAction::Set {
            id: run_identity.run_id.clone(),
            status,
            done_reason,
            updated_at: current_unix_millis(),
        },
    )];
    let patches = reduce_state_actions(actions, &base_state, "agent_loop", &ScopeContext::run())
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    run_ctx.add_thread_patches(patches);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CancellationStage {
    Inference,
    ToolExecution,
}

pub(super) const CANCELLATION_INFERENCE_USER_MESSAGE: &str =
    "The previous run was interrupted during inference. Please continue from the current context.";
pub(super) const CANCELLATION_TOOL_USER_MESSAGE: &str =
    "The previous run was interrupted while using tools. Please continue from the current context.";

pub(super) fn append_cancellation_user_message(run_ctx: &mut RunContext, stage: CancellationStage) {
    let content = match stage {
        CancellationStage::Inference => CANCELLATION_INFERENCE_USER_MESSAGE,
        CancellationStage::ToolExecution => CANCELLATION_TOOL_USER_MESSAGE,
    };
    run_ctx.add_message(Arc::new(Message::user(content)));
}

/// Merge per-inference override options on top of the agent's base `ChatOptions`.
///
/// Returns the merged options, or the agent's base options if no override is set.
pub(super) fn apply_inference_override(
    base: Option<&genai::chat::ChatOptions>,
    ovr: Option<&tirea_contract::runtime::inference::InferenceOverride>,
) -> Option<genai::chat::ChatOptions> {
    let Some(ovr) = ovr else {
        return base.cloned();
    };
    let has_option_overrides = ovr.temperature.is_some()
        || ovr.max_tokens.is_some()
        || ovr.top_p.is_some()
        || ovr.reasoning_effort.is_some();
    if !has_option_overrides {
        return base.cloned();
    }
    let mut opts = base.cloned().unwrap_or_default();
    if let Some(t) = ovr.temperature {
        opts.temperature = Some(t);
    }
    if let Some(t) = ovr.max_tokens {
        opts.max_tokens = Some(t);
    }
    if let Some(t) = ovr.top_p {
        opts.top_p = Some(t);
    }
    if let Some(ref r) = ovr.reasoning_effort {
        opts.reasoning_effort = Some(to_genai_reasoning_effort(r));
    }
    Some(opts)
}

fn to_genai_reasoning_effort(
    r: &tirea_contract::runtime::inference::ReasoningEffort,
) -> genai::chat::ReasoningEffort {
    use tirea_contract::runtime::inference::ReasoningEffort as R;
    match r {
        R::None => genai::chat::ReasoningEffort::None,
        R::Low => genai::chat::ReasoningEffort::Low,
        R::Medium => genai::chat::ReasoningEffort::Medium,
        R::High => genai::chat::ReasoningEffort::High,
        R::Max => genai::chat::ReasoningEffort::Max,
        R::Budget(n) => genai::chat::ReasoningEffort::Budget(*n),
    }
}

pub(super) fn effective_llm_models(agent: &dyn Agent) -> Vec<String> {
    let mut models = Vec::with_capacity(1 + agent.fallback_models().len());
    models.push(agent.model().to_string());
    for model in agent.fallback_models() {
        if model.trim().is_empty() {
            continue;
        }
        if !models.iter().any(|m| m == model) {
            models.push(model.clone());
        }
    }
    models
}

pub(super) fn effective_llm_models_from(
    agent: &dyn Agent,
    start_model: Option<&str>,
) -> Vec<String> {
    let models = effective_llm_models(agent);
    let Some(start_model) = start_model.map(str::trim).filter(|model| !model.is_empty()) else {
        return models;
    };
    let Some(index) = models.iter().position(|model| model == start_model) else {
        return models;
    };
    models.into_iter().skip(index).collect()
}

pub(super) fn effective_llm_models_from_override(
    ovr: &tirea_contract::runtime::inference::InferenceOverride,
) -> Option<Vec<String>> {
    let model = ovr.model.as_ref()?;
    let fallbacks = ovr.fallback_models.as_deref().unwrap_or(&[]);
    let mut models = Vec::with_capacity(1 + fallbacks.len());
    models.push(model.clone());
    for m in fallbacks {
        if m.trim().is_empty() {
            continue;
        }
        if !models.iter().any(|existing| existing == m) {
            models.push(m.clone());
        }
    }
    Some(models)
}

pub(super) fn next_llm_model_after(agent: &dyn Agent, current_model: &str) -> Option<String> {
    let models = effective_llm_models(agent);
    let current_index = models.iter().position(|model| model == current_model)?;
    models.into_iter().nth(current_index + 1)
}

pub(super) fn llm_retry_attempts(agent: &dyn Agent) -> usize {
    agent.llm_retry_policy().max_attempts_per_model.max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LlmErrorClass {
    RateLimit,
    Timeout,
    Connection,
    ServerUnavailable,
    ServerError,
    Auth,
    ClientRequest,
    Unknown,
}

impl LlmErrorClass {
    pub(super) fn is_retryable(self) -> bool {
        matches!(
            self,
            LlmErrorClass::RateLimit
                | LlmErrorClass::Timeout
                | LlmErrorClass::Connection
                | LlmErrorClass::ServerUnavailable
                | LlmErrorClass::ServerError
        )
    }

    /// Stable string label for telemetry and structured logging.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            LlmErrorClass::RateLimit => "rate_limit",
            LlmErrorClass::Timeout => "timeout",
            LlmErrorClass::Connection => "connection",
            LlmErrorClass::ServerUnavailable => "server_unavailable",
            LlmErrorClass::ServerError => "server_error",
            LlmErrorClass::Auth => "auth",
            LlmErrorClass::ClientRequest => "client_request",
            LlmErrorClass::Unknown => "unknown",
        }
    }
}

fn classify_llm_error_message(message: &str) -> LlmErrorClass {
    let lower = message.to_ascii_lowercase();
    if ["429", "too many requests", "rate limit"]
        .iter()
        .any(|p| lower.contains(p))
    {
        return LlmErrorClass::RateLimit;
    }
    if ["timeout", "timed out"].iter().any(|p| lower.contains(p)) {
        return LlmErrorClass::Timeout;
    }
    if [
        "connection",
        "network",
        "reset by peer",
        "broken pipe",
        "eof",
        "connection refused",
        "error sending request for url",
    ]
    .iter()
    .any(|p| lower.contains(p))
    {
        return LlmErrorClass::Connection;
    }
    if ["503", "service unavailable", "unavailable", "temporar"]
        .iter()
        .any(|p| lower.contains(p))
    {
        return LlmErrorClass::ServerUnavailable;
    }
    if [
        "500",
        "502",
        "504",
        "server error",
        "bad gateway",
        "gateway timeout",
    ]
    .iter()
    .any(|p| lower.contains(p))
    {
        return LlmErrorClass::ServerError;
    }
    if ["401", "403", "unauthorized", "forbidden", "invalid api key"]
        .iter()
        .any(|p| lower.contains(p))
    {
        return LlmErrorClass::Auth;
    }
    if ["400", "404", "422", "invalid_request", "bad request"]
        .iter()
        .any(|p| lower.contains(p))
    {
        return LlmErrorClass::ClientRequest;
    }
    LlmErrorClass::Unknown
}

fn classify_status_code(status_code: u16) -> LlmErrorClass {
    match status_code {
        408 => LlmErrorClass::Timeout,
        429 => LlmErrorClass::RateLimit,
        401 | 403 => LlmErrorClass::Auth,
        400 | 404 | 422 => LlmErrorClass::ClientRequest,
        503 => LlmErrorClass::ServerUnavailable,
        500..=599 => LlmErrorClass::ServerError,
        400..=499 => LlmErrorClass::ClientRequest,
        _ => LlmErrorClass::Unknown,
    }
}

fn classify_error_chain(error: &(dyn std::error::Error + 'static)) -> Option<LlmErrorClass> {
    let mut current = Some(error);
    while let Some(err) = current {
        if let Some(io_error) = err.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind;

            let class = match io_error.kind() {
                ErrorKind::TimedOut => Some(LlmErrorClass::Timeout),
                ErrorKind::ConnectionAborted
                | ErrorKind::ConnectionRefused
                | ErrorKind::ConnectionReset
                | ErrorKind::BrokenPipe
                | ErrorKind::NotConnected
                | ErrorKind::UnexpectedEof => Some(LlmErrorClass::Connection),
                _ => None,
            };
            if class.is_some() {
                return class;
            }
        }
        current = err.source();
    }
    None
}

fn classify_webc_error(error: &genai::webc::Error) -> LlmErrorClass {
    match error {
        genai::webc::Error::ResponseFailedStatus { status, .. } => {
            classify_status_code(status.as_u16())
        }
        genai::webc::Error::Reqwest(err) => classify_error_chain(err)
            .unwrap_or_else(|| classify_llm_error_message(&err.to_string())),
        _ => classify_llm_error_message(&error.to_string()),
    }
}

/// Classify a provider error event body by parsing its structured fields.
///
/// Provider error events (OpenAI, Anthropic, Gemini) embed a JSON body with a
/// `type` field that describes the error category. This function extracts that
/// field and maps it to an [`LlmErrorClass`] without relying on keyword
/// matching against stringified content.
///
/// Known error type strings:
/// - OpenAI: `"server_error"`, `"rate_limit_error"`, `"invalid_request_error"`, …
/// - Anthropic: `"overloaded_error"`, `"api_error"`, `"authentication_error"`, …
fn classify_chat_response_body(body: &serde_json::Value) -> LlmErrorClass {
    // Collect candidate type strings from both the top-level and nested error
    // object.  OpenAI streams extract the inner error, so body["type"] is
    // already the error type.  Anthropic / Gemini may wrap it in an envelope
    // where body["type"] == "error" and the real type lives at
    // body["error"]["type"].
    let top_type = body.get("type").and_then(|v| v.as_str());
    let nested_type = body
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(|v| v.as_str());

    // Try the nested (more specific) type first when the top-level type is a
    // generic envelope marker like "error".
    let candidates: &[&str] = match (top_type, nested_type) {
        (_, Some(inner)) => &[inner, top_type.unwrap_or("")],
        (Some(top), None) => &[top],
        (None, None) => &[],
    };

    for t in candidates {
        let lower = t.to_ascii_lowercase();
        if lower.contains("rate_limit") {
            return LlmErrorClass::RateLimit;
        }
        if lower.contains("overloaded") || lower.contains("unavailable") {
            return LlmErrorClass::ServerUnavailable;
        }
        if lower.contains("timeout") {
            return LlmErrorClass::Timeout;
        }
        if lower.contains("server_error") || lower.contains("api_error") {
            return LlmErrorClass::ServerError;
        }
        if lower.contains("authentication") || lower.contains("permission") {
            return LlmErrorClass::Auth;
        }
        if lower.contains("invalid_request") || lower.contains("not_found") {
            return LlmErrorClass::ClientRequest;
        }
    }

    // Fallback: check for an HTTP status code in the body (some providers
    // include a numeric `status` or `code` field).
    let status_code = body
        .get("status")
        .or_else(|| body.get("code"))
        .and_then(|v| v.as_u64())
        .and_then(|v| u16::try_from(v).ok());

    if let Some(code) = status_code {
        let class = classify_status_code(code);
        if class != LlmErrorClass::Unknown {
            return class;
        }
    }

    LlmErrorClass::Unknown
}

pub(super) fn classify_llm_error(error: &genai::Error) -> LlmErrorClass {
    match error {
        genai::Error::HttpError { status, .. } => classify_status_code(status.as_u16()),
        genai::Error::WebStream { cause, error, .. } => classify_error_chain(error.as_ref())
            .unwrap_or_else(|| classify_llm_error_message(cause)),
        genai::Error::WebAdapterCall { webc_error, .. }
        | genai::Error::WebModelCall { webc_error, .. } => classify_webc_error(webc_error),
        genai::Error::Internal(message) => classify_llm_error_message(message),

        // Mid-stream JSON parse failure — almost always caused by truncated /
        // corrupted SSE data due to a transient network issue.
        genai::Error::StreamParse { .. } => LlmErrorClass::Connection,

        // Provider returned no response body at all — treat as server-side fault.
        genai::Error::NoChatResponse { .. } => LlmErrorClass::ServerError,

        // Provider streamed an explicit error event with a structured body.
        genai::Error::ChatResponse { body, .. } => classify_chat_response_body(body),

        // Auth configuration errors from genai itself (not HTTP 401/403).
        genai::Error::RequiresApiKey { .. }
        | genai::Error::NoAuthResolver { .. }
        | genai::Error::NoAuthData { .. } => LlmErrorClass::Auth,

        other => classify_llm_error_message(&other.to_string()),
    }
}

#[cfg(test)]
pub(super) fn is_retryable_llm_error(error: &genai::Error) -> bool {
    classify_llm_error(error).is_retryable()
}

static RETRY_BACKOFF_ENTROPY: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

#[derive(Debug, Clone, Default)]
pub(super) struct RetryBackoffWindow {
    started_at: Option<Instant>,
}

impl RetryBackoffWindow {
    pub(super) fn elapsed_ms(&mut self) -> u64 {
        self.started_at
            .get_or_insert_with(Instant::now)
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    pub(super) fn reset(&mut self) {
        self.started_at = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RetryBackoffOutcome {
    Completed,
    Cancelled,
    BudgetExhausted,
}

fn mix_retry_entropy(mut entropy: u64) -> u64 {
    entropy = entropy.wrapping_add(0x9e37_79b9_7f4a_7c15);
    entropy = (entropy ^ (entropy >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    entropy = (entropy ^ (entropy >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    entropy ^ (entropy >> 31)
}

fn next_retry_entropy() -> u64 {
    let counter = RETRY_BACKOFF_ENTROPY.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(counter);
    mix_retry_entropy(counter ^ now)
}

pub(super) fn retry_base_backoff_ms(policy: &LlmRetryPolicy, retry_attempt: usize) -> u64 {
    let initial = policy.initial_backoff_ms;
    let cap = policy.max_backoff_ms.max(policy.initial_backoff_ms);
    let attempt = retry_attempt.max(1);
    if attempt == 1 {
        return initial.min(cap);
    }
    let shift = (attempt - 1).min(20) as u32;
    let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    initial.saturating_mul(factor).min(cap)
}

fn jittered_backoff_ms(policy: &LlmRetryPolicy, retry_attempt: usize, entropy: u64) -> u64 {
    let cap = policy.max_backoff_ms.max(policy.initial_backoff_ms);
    let base_ms = retry_base_backoff_ms(policy, retry_attempt).min(cap);
    let jitter_percent = policy.backoff_jitter_percent.min(100) as u64;
    if jitter_percent == 0 || base_ms == 0 {
        return base_ms;
    }

    let jitter_window = base_ms.saturating_mul(jitter_percent) / 100;
    let lower = base_ms.saturating_sub(jitter_window);
    let upper = base_ms.saturating_add(jitter_window).min(cap);
    if upper <= lower {
        return lower;
    }

    let span = upper - lower;
    lower + (mix_retry_entropy(entropy) % (span.saturating_add(1)))
}

pub(super) fn retry_backoff_plan_ms(
    policy: &LlmRetryPolicy,
    retry_attempt: usize,
    elapsed_ms: u64,
    entropy: u64,
) -> Option<u64> {
    let wait_ms = jittered_backoff_ms(policy, retry_attempt, entropy);
    match policy.max_retry_window_ms {
        Some(budget_ms) if elapsed_ms.saturating_add(wait_ms) > budget_ms => None,
        _ => Some(wait_ms),
    }
}

pub(super) async fn wait_retry_backoff(
    agent: &dyn Agent,
    retry_attempt: usize,
    retry_window: &mut RetryBackoffWindow,
    run_cancellation_token: Option<&RunCancellationToken>,
) -> RetryBackoffOutcome {
    let elapsed_ms = retry_window.elapsed_ms();
    let Some(wait_ms) = retry_backoff_plan_ms(
        agent.llm_retry_policy(),
        retry_attempt,
        elapsed_ms,
        next_retry_entropy(),
    ) else {
        return RetryBackoffOutcome::BudgetExhausted;
    };
    tracing::debug!(
        attempt = retry_attempt,
        backoff_ms = wait_ms,
        elapsed_ms = elapsed_ms,
        "waiting before LLM retry"
    );
    match await_or_cancel(
        run_cancellation_token,
        tokio::time::sleep(std::time::Duration::from_millis(wait_ms)),
    )
    .await
    {
        CancelAware::Cancelled => RetryBackoffOutcome::Cancelled,
        CancelAware::Value(_) => RetryBackoffOutcome::Completed,
    }
}

pub(super) enum LlmAttemptOutcome<T> {
    Success {
        value: T,
        model: String,
        attempts: usize,
    },
    Cancelled,
    Exhausted {
        last_error: String,
        last_error_class: Option<&'static str>,
        attempts: usize,
    },
}

fn is_run_cancelled(token: Option<&RunCancellationToken>) -> bool {
    is_cancelled(token)
}

pub(super) fn step_tool_provider_for_run(
    agent: &dyn Agent,
    tools: HashMap<String, Arc<dyn Tool>>,
) -> Arc<dyn StepToolProvider> {
    agent.step_tool_provider().unwrap_or_else(|| {
        Arc::new(StaticStepToolProvider::new(tools)) as Arc<dyn StepToolProvider>
    })
}

pub(super) fn llm_executor_for_run(agent: &dyn Agent) -> Arc<dyn LlmExecutor> {
    agent
        .llm_executor()
        .unwrap_or_else(|| Arc::new(GenaiLlmExecutor::new(Client::default())))
}

pub(super) async fn resolve_step_tool_snapshot(
    step_tool_provider: &Arc<dyn StepToolProvider>,
    run_ctx: &RunContext,
) -> Result<StepToolSnapshot, AgentLoopError> {
    step_tool_provider
        .provide(StepToolInput { state: run_ctx })
        .await
}

fn mark_step_completed(run_state: &mut LoopRunState) {
    run_state.completed_steps += 1;
}

fn stitch_response_text(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        return segment.to_string();
    }
    if segment.is_empty() {
        return prefix.to_string();
    }
    let mut stitched = String::with_capacity(prefix.len() + segment.len());
    stitched.push_str(prefix);
    stitched.push_str(segment);
    stitched
}

fn extend_response_prefix(prefix: &mut String, segment: &str) {
    if !segment.is_empty() {
        prefix.push_str(segment);
    }
}

fn build_loop_outcome(
    run_ctx: RunContext,
    termination: TerminationReason,
    response: Option<String>,
    run_state: &LoopRunState,
    failure: Option<outcome::LoopFailure>,
) -> LoopOutcome {
    LoopOutcome {
        run_ctx,
        termination,
        response: response.filter(|text| !text.is_empty()),
        usage: run_state.usage(),
        stats: run_state.stats(),
        failure,
    }
}

pub(super) async fn run_llm_with_retry_and_fallback<T, Invoke, Fut>(
    agent: &dyn Agent,
    run_cancellation_token: Option<&RunCancellationToken>,
    retry_current_model: bool,
    start_model: Option<&str>,
    inference_override: Option<&tirea_contract::runtime::inference::InferenceOverride>,
    unknown_error: &str,
    mut invoke: Invoke,
) -> LlmAttemptOutcome<T>
where
    Invoke: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = genai::Result<T>>,
{
    let mut last_llm_error = unknown_error.to_string();
    let mut last_error_class: Option<&'static str> = None;
    let model_candidates = match inference_override.and_then(effective_llm_models_from_override) {
        Some(base) => match start_model.map(str::trim).filter(|s| !s.is_empty()) {
            Some(sm) => {
                if let Some(idx) = base.iter().position(|m| m == sm) {
                    base.into_iter().skip(idx).collect()
                } else {
                    base
                }
            }
            None => base,
        },
        None => effective_llm_models_from(agent, start_model),
    };
    let max_attempts = llm_retry_attempts(agent);
    let mut total_attempts = 0usize;
    let mut retry_window = RetryBackoffWindow::default();

    'models: for model in model_candidates {
        for attempt in 1..=max_attempts {
            total_attempts = total_attempts.saturating_add(1);
            let response_res =
                match await_or_cancel(run_cancellation_token, invoke(model.clone())).await {
                    CancelAware::Cancelled => return LlmAttemptOutcome::Cancelled,
                    CancelAware::Value(resp) => resp,
                };

            match response_res {
                Ok(value) => {
                    if total_attempts > 1 {
                        tracing::info!(
                            model = %model,
                            attempts = total_attempts,
                            "LLM call succeeded after retries"
                        );
                    }
                    return LlmAttemptOutcome::Success {
                        value,
                        model,
                        attempts: total_attempts,
                    };
                }
                Err(e) => {
                    let error_class = classify_llm_error(&e);
                    last_error_class = Some(error_class.as_str());
                    let message = e.to_string();
                    last_llm_error =
                        format!("model='{model}' attempt={attempt}/{max_attempts}: {message}");
                    let can_retry_same_model =
                        retry_current_model && attempt < max_attempts && error_class.is_retryable();
                    tracing::warn!(
                        model = %model,
                        attempt = attempt,
                        max_attempts = max_attempts,
                        error_class = error_class.as_str(),
                        retryable = can_retry_same_model,
                        error = %message,
                        "LLM call failed"
                    );
                    if can_retry_same_model {
                        match wait_retry_backoff(
                            agent,
                            attempt,
                            &mut retry_window,
                            run_cancellation_token,
                        )
                        .await
                        {
                            RetryBackoffOutcome::Completed => continue,
                            RetryBackoffOutcome::Cancelled => {
                                return LlmAttemptOutcome::Cancelled;
                            }
                            RetryBackoffOutcome::BudgetExhausted => {
                                tracing::warn!(
                                    model = %model,
                                    attempt = attempt,
                                    "LLM retry budget exhausted"
                                );
                                last_llm_error =
                                    format!("{last_llm_error} (retry budget exhausted)");
                                break 'models;
                            }
                        }
                    }
                    break;
                }
            }
        }
    }

    LlmAttemptOutcome::Exhausted {
        last_error: last_llm_error,
        last_error_class,
        attempts: total_attempts,
    }
}

pub(super) async fn run_step_prepare_phases(
    run_ctx: &RunContext,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    agent: &dyn Agent,
) -> Result<
    (
        Vec<Message>,
        Vec<String>,
        RunAction,
        Vec<std::sync::Arc<dyn tirea_contract::runtime::inference::InferenceRequestTransform>>,
        Option<tirea_contract::runtime::inference::InferenceOverride>,
        Vec<TrackedPatch>,
        Vec<tirea_contract::SerializedStateAction>,
    ),
    AgentLoopError,
> {
    let system_prompt = agent.system_prompt().to_string();
    let ((messages, filtered_tools, run_action, transforms, inference_override), pending, actions) =
        plugin_runtime::run_phase_block(
            run_ctx,
            tool_descriptors,
            agent,
            &[Phase::StepStart, Phase::BeforeInference],
            |_| {},
            |step| inference_inputs_from_step(step, &system_prompt),
        )
        .await?;
    Ok((
        messages,
        filtered_tools,
        run_action,
        transforms,
        inference_override,
        pending,
        actions,
    ))
}

pub(super) struct PreparedStep {
    pub(super) messages: Vec<Message>,
    pub(super) filtered_tools: Vec<String>,
    pub(super) run_action: RunAction,
    pub(super) pending_patches: Vec<TrackedPatch>,
    pub(super) serialized_state_actions: Vec<tirea_contract::SerializedStateAction>,
    pub(super) request_transforms:
        Vec<std::sync::Arc<dyn tirea_contract::runtime::inference::InferenceRequestTransform>>,
    pub(super) inference_override: Option<tirea_contract::runtime::inference::InferenceOverride>,
}

pub(super) async fn prepare_step_execution(
    run_ctx: &RunContext,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    agent: &dyn Agent,
) -> Result<PreparedStep, AgentLoopError> {
    let (messages, filtered_tools, run_action, transforms, inference_override, pending, actions) =
        run_step_prepare_phases(run_ctx, tool_descriptors, agent).await?;
    Ok(PreparedStep {
        messages,
        filtered_tools,
        run_action,
        pending_patches: pending,
        serialized_state_actions: actions,
        request_transforms: transforms,
        inference_override,
    })
}

pub(super) async fn apply_llm_error_cleanup(
    run_ctx: &mut RunContext,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    agent: &dyn Agent,
    error_type: &'static str,
    message: String,
    error_class: Option<&str>,
) -> Result<(), AgentLoopError> {
    plugin_runtime::emit_cleanup_phases(
        run_ctx,
        tool_descriptors,
        agent,
        error_type,
        message,
        error_class,
    )
    .await
}

pub(super) async fn complete_step_after_inference(
    run_ctx: &mut RunContext,
    result: &StreamResult,
    step_meta: MessageMetadata,
    assistant_message_id: Option<String>,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    agent: &dyn Agent,
) -> Result<RunAction, AgentLoopError> {
    let (run_action, pending, actions) = plugin_runtime::run_phase_block(
        run_ctx,
        tool_descriptors,
        agent,
        &[Phase::AfterInference],
        |step| {
            use crate::contracts::runtime::inference::LLMResponse;
            step.llm_response = Some(LLMResponse::success(result.clone()));
        },
        |step| step.run_action(),
    )
    .await?;
    run_ctx.add_thread_patches(pending);
    run_ctx.add_serialized_state_actions(actions);

    let assistant = assistant_turn_message(result, step_meta, assistant_message_id);
    run_ctx.add_message(Arc::new(assistant));

    let (pending, actions) =
        plugin_runtime::emit_phase_block(Phase::StepEnd, run_ctx, tool_descriptors, agent, |_| {})
            .await?;
    run_ctx.add_thread_patches(pending);
    run_ctx.add_serialized_state_actions(actions);
    Ok(run_action)
}

/// Emit events for a pending tool-call projection.
pub(super) fn pending_tool_events(call: &SuspendedCall) -> Vec<AgentEvent> {
    vec![
        AgentEvent::ToolCallStart {
            id: call.ticket.pending.id.clone(),
            name: call.ticket.pending.name.clone(),
        },
        AgentEvent::ToolCallReady {
            id: call.ticket.pending.id.clone(),
            name: call.ticket.pending.name.clone(),
            arguments: call.ticket.pending.arguments.clone(),
        },
    ]
}

pub(super) fn has_suspended_calls(run_ctx: &RunContext) -> bool {
    !suspended_calls_from_ctx(run_ctx).is_empty()
}

pub(super) fn suspended_call_ids(run_ctx: &RunContext) -> HashSet<String> {
    suspended_calls_from_ctx(run_ctx).into_keys().collect()
}

pub(super) fn newly_suspended_call_ids(
    run_ctx: &RunContext,
    baseline_ids: &HashSet<String>,
) -> HashSet<String> {
    suspended_calls_from_ctx(run_ctx)
        .into_keys()
        .filter(|id| !baseline_ids.contains(id))
        .collect()
}

pub(super) fn suspended_call_pending_events(run_ctx: &RunContext) -> Vec<AgentEvent> {
    let mut calls: Vec<SuspendedCall> = suspended_calls_from_ctx(run_ctx).into_values().collect();
    calls.sort_by(|left, right| left.call_id.cmp(&right.call_id));
    calls
        .into_iter()
        .flat_map(|call| pending_tool_events(&call))
        .collect()
}

pub(super) fn suspended_call_pending_events_for_ids(
    run_ctx: &RunContext,
    call_ids: &HashSet<String>,
) -> Vec<AgentEvent> {
    if call_ids.is_empty() {
        return Vec::new();
    }
    let mut calls: Vec<SuspendedCall> = suspended_calls_from_ctx(run_ctx)
        .into_iter()
        .filter_map(|(call_id, call)| call_ids.contains(&call_id).then_some(call))
        .collect();
    calls.sort_by(|left, right| left.call_id.cmp(&right.call_id));
    calls
        .into_iter()
        .flat_map(|call| pending_tool_events(&call))
        .collect()
}

pub(super) struct ToolExecutionContext {
    pub(super) state: serde_json::Value,
    pub(super) run_policy: tirea_contract::RunPolicy,
    pub(super) run_identity: RunIdentity,
    pub(super) caller_context: crate::contracts::runtime::tool_call::CallerContext,
}

pub(super) fn prepare_tool_execution_context(
    run_ctx: &RunContext,
) -> Result<ToolExecutionContext, AgentLoopError> {
    let state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let caller_context = caller_context_for_tool_execution(run_ctx, &state);
    Ok(ToolExecutionContext {
        state,
        run_policy: run_ctx.run_policy().clone(),
        run_identity: run_ctx.run_identity().clone(),
        caller_context,
    })
}

pub(super) async fn finalize_run_end(
    run_ctx: &mut RunContext,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    agent: &dyn Agent,
) {
    plugin_runtime::emit_run_end_phase(run_ctx, tool_descriptors, agent).await
}

fn normalize_termination_for_suspended_calls(
    run_ctx: &RunContext,
    termination: TerminationReason,
    response: Option<String>,
) -> (TerminationReason, Option<String>) {
    let final_termination = if !matches!(
        termination,
        TerminationReason::Error(_) | TerminationReason::Cancelled
    ) && has_suspended_calls(run_ctx)
    {
        TerminationReason::Suspended
    } else {
        termination
    };
    let final_response = if final_termination == TerminationReason::Suspended {
        None
    } else {
        response
    };
    (final_termination, final_response)
}

async fn persist_run_termination(
    run_ctx: &mut RunContext,
    termination: &TerminationReason,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    agent: &dyn Agent,
    run_identity: &RunIdentity,
    pending_delta_commit: &PendingDeltaCommitContext<'_>,
    token_totals: RunTokenTotals,
) -> Result<(), AgentLoopError> {
    sync_run_lifecycle_for_termination_with_context(run_ctx, run_identity, termination)?;
    finalize_run_end(run_ctx, tool_descriptors, agent).await;
    pending_delta_commit
        .commit_run_finished(run_ctx, termination, token_totals)
        .await?;
    Ok(())
}

fn stream_result_from_chat_response(response: &genai::chat::ChatResponse) -> StreamResult {
    let text = response
        .first_text()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let tool_calls: Vec<crate::contracts::thread::ToolCall> = response
        .tool_calls()
        .into_iter()
        .map(|tc| {
            crate::contracts::thread::ToolCall::new(
                &tc.call_id,
                &tc.fn_name,
                tc.fn_arguments.clone(),
            )
        })
        .collect();

    let usage = Some(crate::runtime::streaming::token_usage_from_genai(
        &response.usage,
    ));
    let stop_reason = response
        .stop_reason
        .as_ref()
        .and_then(crate::runtime::streaming::map_genai_stop_reason)
        .or({
            if !tool_calls.is_empty() {
                Some(tirea_contract::runtime::inference::StopReason::ToolUse)
            } else {
                Some(tirea_contract::runtime::inference::StopReason::EndTurn)
            }
        });
    StreamResult {
        text,
        tool_calls,
        usage,
        stop_reason,
    }
}

fn assistant_turn_message(
    result: &StreamResult,
    step_meta: MessageMetadata,
    message_id: Option<String>,
) -> Message {
    let mut msg = if result.tool_calls.is_empty() {
        assistant_message(&result.text)
    } else {
        assistant_tool_calls(&result.text, result.tool_calls.clone())
    }
    .with_metadata(step_meta);
    if let Some(message_id) = message_id {
        msg = msg.with_id(message_id);
    }
    msg
}

struct RunStartDrainOutcome {
    events: Vec<AgentEvent>,
    replayed: bool,
}

fn decision_result_value(action: &ResumeDecisionAction, result: &Value) -> serde_json::Value {
    if result.is_null() {
        serde_json::Value::Bool(matches!(action, ResumeDecisionAction::Resume))
    } else {
        result.clone()
    }
}

fn runtime_resume_inputs(run_ctx: &RunContext) -> HashMap<String, ToolCallResume> {
    let mut decisions = HashMap::new();
    for (call_id, state) in tool_call_states_from_ctx(run_ctx) {
        if !matches!(state.status, ToolCallStatus::Resuming) {
            continue;
        }
        let Some(mut resume) = state.resume else {
            continue;
        };
        if resume.decision_id.trim().is_empty() {
            resume.decision_id = call_id.clone();
        }
        decisions.insert(call_id, resume);
    }
    decisions
}

fn settle_orphan_resuming_tool_states(
    run_ctx: &mut RunContext,
    suspended: &HashMap<String, SuspendedCall>,
    resumes: &HashMap<String, ToolCallResume>,
) -> Result<bool, AgentLoopError> {
    let states = tool_call_states_from_ctx(run_ctx);
    let mut changed = false;

    for (call_id, resume) in resumes {
        if suspended.contains_key(call_id) {
            continue;
        }
        let Some(state) = states.get(call_id).cloned() else {
            continue;
        };
        let target_status = match &resume.action {
            ResumeDecisionAction::Cancel => ToolCallStatus::Cancelled,
            ResumeDecisionAction::Resume => ToolCallStatus::Failed,
        };
        if state.status == target_status && state.resume.as_ref() == Some(resume) {
            continue;
        }

        let Some(next_state) = transition_tool_call_state(
            Some(state.clone()),
            ToolCallStateSeed {
                call_id: call_id.as_str(),
                tool_name: state.tool_name.as_str(),
                arguments: &state.arguments,
                status: state.status,
                resume_token: state.resume_token.clone(),
            },
            ToolCallStateTransition {
                status: target_status,
                resume_token: state.resume_token.clone(),
                resume: Some(resume.clone()),
                updated_at: current_unix_millis(),
            },
        ) else {
            continue;
        };

        let base_state = run_ctx
            .snapshot()
            .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
        let patch = upsert_tool_call_state(&base_state, call_id, next_state)?;
        if patch.patch().is_empty() {
            continue;
        }
        run_ctx.add_thread_patch(patch);
        changed = true;
    }

    Ok(changed)
}

fn all_suspended_calls_have_resume(
    suspended: &HashMap<String, SuspendedCall>,
    resumes: &HashMap<String, ToolCallResume>,
) -> bool {
    suspended
        .keys()
        .all(|call_id| resumes.contains_key(call_id))
}

async fn drain_resuming_tool_calls_and_replay(
    run_ctx: &mut RunContext,
    tools: &HashMap<String, Arc<dyn Tool>>,
    agent: &dyn Agent,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
) -> Result<RunStartDrainOutcome, AgentLoopError> {
    let decisions = runtime_resume_inputs(run_ctx);
    if decisions.is_empty() {
        return Ok(RunStartDrainOutcome {
            events: Vec::new(),
            replayed: false,
        });
    }

    let suspended = suspended_calls_from_ctx(run_ctx);
    let mut state_changed = false;
    if settle_orphan_resuming_tool_states(run_ctx, &suspended, &decisions)? {
        state_changed = true;
    }
    if suspended.is_empty() {
        if state_changed {
            let snapshot = run_ctx
                .snapshot()
                .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
            return Ok(RunStartDrainOutcome {
                events: vec![AgentEvent::StateSnapshot { snapshot }],
                replayed: false,
            });
        }
        return Ok(RunStartDrainOutcome {
            events: Vec::new(),
            replayed: false,
        });
    }

    if matches!(
        agent.tool_executor().decision_replay_policy(),
        DecisionReplayPolicy::BatchAllSuspended
    ) && !all_suspended_calls_have_resume(&suspended, &decisions)
    {
        if state_changed {
            let snapshot = run_ctx
                .snapshot()
                .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
            return Ok(RunStartDrainOutcome {
                events: vec![AgentEvent::StateSnapshot { snapshot }],
                replayed: false,
            });
        }
        return Ok(RunStartDrainOutcome {
            events: Vec::new(),
            replayed: false,
        });
    }

    let mut events = Vec::new();
    let mut decision_ids: Vec<String> = decisions.keys().cloned().collect();
    decision_ids.sort();

    let mut replayed = false;

    for call_id in decision_ids {
        let Some(suspended_call) = suspended.get(&call_id).cloned() else {
            continue;
        };
        let Some(decision) = decisions.get(&call_id).cloned() else {
            continue;
        };
        replayed = true;
        let decision_result = decision_result_value(&decision.action, &decision.result);
        let resume_payload = ToolCallResume {
            result: decision_result.clone(),
            ..decision.clone()
        };
        events.push(AgentEvent::ToolCallResumed {
            target_id: suspended_call.call_id.clone(),
            result: decision_result.clone(),
        });

        match decision.action {
            ResumeDecisionAction::Cancel => {
                let cancel_reason = resume_payload.reason.clone();
                if upsert_tool_call_lifecycle_state(
                    run_ctx,
                    &suspended_call,
                    ToolCallStatus::Cancelled,
                    Some(resume_payload),
                )? {
                    state_changed = true;
                }
                events.push(append_denied_tool_result_message(
                    run_ctx,
                    &suspended_call.call_id,
                    Some(&suspended_call.tool_name),
                    cancel_reason.as_deref(),
                ));
                // Cancel path skips tool execution, so no automatic scope
                // cleanup runs. Delete just the suspended_call entry; keep
                // tool_call_state (Cancelled status) for audit.
                let cleanup_path = format!(
                    "__tool_call_scope.{}.suspended_call",
                    suspended_call.call_id
                );
                let cleanup_patch = tirea_state::Patch::with_ops(vec![tirea_state::Op::delete(
                    tirea_state::parse_path(&cleanup_path),
                )]);
                let tracked = tirea_state::TrackedPatch::new(cleanup_patch)
                    .with_source("framework:scope_cleanup");
                if !tracked.patch().is_empty() {
                    state_changed = true;
                    run_ctx.add_thread_patch(tracked);
                }
            }
            ResumeDecisionAction::Resume => {
                if upsert_tool_call_lifecycle_state(
                    run_ctx,
                    &suspended_call,
                    ToolCallStatus::Resuming,
                    Some(resume_payload.clone()),
                )? {
                    state_changed = true;
                }
                let Some(tool_call) = replay_tool_call_for_resolution(
                    run_ctx,
                    &suspended_call,
                    &ToolCallDecision {
                        target_id: suspended_call.call_id.clone(),
                        resume: resume_payload.clone(),
                    },
                ) else {
                    continue;
                };
                let state = run_ctx
                    .snapshot()
                    .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
                let tool = tools.get(&tool_call.name).cloned();
                let caller_context = caller_context_for_tool_execution(run_ctx, &state);
                let replay_phase_ctx = ToolPhaseContext {
                    tool_descriptors,
                    agent_behavior: Some(agent.behavior()),
                    activity_manager: tirea_contract::runtime::activity::NoOpActivityManager::arc(),
                    run_policy: run_ctx.run_policy(),
                    run_identity: run_ctx.run_identity().clone(),
                    caller_context,
                    thread_id: run_ctx.thread_id(),
                    thread_messages: run_ctx.messages(),
                    cancellation_token: None,
                };
                let replay_result = execute_single_tool_with_phases_deferred(
                    tool.as_deref(),
                    &tool_call,
                    &state,
                    &replay_phase_ctx,
                )
                .await?;

                let replay_msg_id = gen_message_id();
                let replay_msg = tool_response(&tool_call.id, &replay_result.execution.result)
                    .with_id(replay_msg_id.clone());
                run_ctx.add_message(Arc::new(replay_msg));

                if !replay_result.reminders.is_empty() {
                    let msgs: Vec<Arc<Message>> = replay_result
                        .reminders
                        .iter()
                        .map(|reminder| {
                            Arc::new(Message::internal_system(format!(
                                "<system-reminder>{}</system-reminder>",
                                reminder
                            )))
                        })
                        .collect();
                    run_ctx.add_messages(msgs);
                }

                if let Some(patch) = replay_result.execution.patch.clone() {
                    state_changed = true;
                    run_ctx.add_thread_patch(patch);
                }
                if !replay_result.pending_patches.is_empty() {
                    state_changed = true;
                    run_ctx.add_thread_patches(replay_result.pending_patches.clone());
                }
                events.push(AgentEvent::ToolCallDone {
                    id: tool_call.id.clone(),
                    result: replay_result.execution.result,
                    patch: replay_result.execution.patch,
                    message_id: replay_msg_id,
                });

                if let Some(next_suspended_call) = replay_result.suspended_call.clone() {
                    let state = run_ctx
                        .snapshot()
                        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
                    let action = next_suspended_call.clone().into_state_action();
                    let patches = reduce_state_actions(
                        vec![action],
                        &state,
                        "agent_loop",
                        &ScopeContext::run(),
                    )
                    .map_err(|e| {
                        AgentLoopError::StateError(format!(
                            "failed to reduce suspended call action: {e}"
                        ))
                    })?;
                    for patch in patches {
                        if !patch.patch().is_empty() {
                            state_changed = true;
                            run_ctx.add_thread_patch(patch);
                        }
                    }
                    for event in pending_tool_events(&next_suspended_call) {
                        events.push(event);
                    }
                }
            }
        }
    }

    // No explicit clear_suspended_call needed: terminal-outcome tool calls
    // clear `__tool_call_scope.<call_id>.suspended_call` automatically in
    // execute_single_tool_with_phases_impl while preserving tool_call_state.

    if state_changed {
        let snapshot = run_ctx
            .snapshot()
            .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
        events.push(AgentEvent::StateSnapshot { snapshot });
    }

    Ok(RunStartDrainOutcome { events, replayed })
}

async fn drain_run_start_resume_replay(
    run_ctx: &mut RunContext,
    tools: &HashMap<String, Arc<dyn Tool>>,
    agent: &dyn Agent,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
) -> Result<RunStartDrainOutcome, AgentLoopError> {
    drain_resuming_tool_calls_and_replay(run_ctx, tools, agent, tool_descriptors).await
}

async fn commit_run_start_and_drain_replay(
    run_ctx: &mut RunContext,
    tools: &HashMap<String, Arc<dyn Tool>>,
    agent: &dyn Agent,
    active_tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    pending_delta_commit: &PendingDeltaCommitContext<'_>,
) -> Result<RunStartDrainOutcome, AgentLoopError> {
    pending_delta_commit
        .commit(run_ctx, CheckpointReason::UserMessage, false)
        .await?;

    let run_start_drain =
        drain_run_start_resume_replay(run_ctx, tools, agent, active_tool_descriptors).await?;

    if run_start_drain.replayed {
        pending_delta_commit
            .commit(run_ctx, CheckpointReason::ToolResultsCommitted, false)
            .await?;
    }

    Ok(run_start_drain)
}

fn normalize_decision_tool_result(
    response: &serde_json::Value,
    fallback_arguments: &serde_json::Value,
) -> serde_json::Value {
    match response {
        serde_json::Value::Bool(_) => fallback_arguments.clone(),
        value => value.clone(),
    }
}

fn denied_tool_result_for_call(
    run_ctx: &RunContext,
    call_id: &str,
    fallback_tool_name: Option<&str>,
    decision_reason: Option<&str>,
) -> ToolResult {
    let tool_name = fallback_tool_name
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| find_tool_call_in_messages(run_ctx, call_id).map(|call| call.name))
        .unwrap_or_else(|| "tool".to_string());
    let reason = decision_reason
        .map(str::to_string)
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| "User denied the action".to_string());
    ToolResult::error(tool_name, reason)
}

fn append_denied_tool_result_message(
    run_ctx: &mut RunContext,
    call_id: &str,
    fallback_tool_name: Option<&str>,
    decision_reason: Option<&str>,
) -> AgentEvent {
    let denied_result =
        denied_tool_result_for_call(run_ctx, call_id, fallback_tool_name, decision_reason);
    let message_id = gen_message_id();
    let denied_message = tool_response(call_id, &denied_result).with_id(message_id.clone());
    run_ctx.add_message(Arc::new(denied_message));
    AgentEvent::ToolCallDone {
        id: call_id.to_string(),
        result: denied_result,
        patch: None,
        message_id,
    }
}

fn find_tool_call_in_messages(run_ctx: &RunContext, call_id: &str) -> Option<ToolCall> {
    run_ctx.messages().iter().rev().find_map(|message| {
        message
            .tool_calls
            .as_ref()
            .and_then(|calls| calls.iter().find(|call| call.id == call_id).cloned())
    })
}

fn replay_tool_call_for_resolution(
    _run_ctx: &RunContext,
    suspended_call: &SuspendedCall,
    decision: &ToolCallDecision,
) -> Option<ToolCall> {
    if matches!(decision.resume.action, ResumeDecisionAction::Cancel) {
        return None;
    }

    match suspended_call.ticket.resume_mode {
        ToolCallResumeMode::ReplayToolCall => Some(ToolCall::new(
            suspended_call.call_id.clone(),
            suspended_call.tool_name.clone(),
            suspended_call.arguments.clone(),
        )),
        ToolCallResumeMode::UseDecisionAsToolResult | ToolCallResumeMode::PassDecisionToTool => {
            Some(ToolCall::new(
                suspended_call.call_id.clone(),
                suspended_call.tool_name.clone(),
                normalize_decision_tool_result(&decision.resume.result, &suspended_call.arguments),
            ))
        }
    }
}

fn upsert_tool_call_lifecycle_state(
    run_ctx: &mut RunContext,
    suspended_call: &SuspendedCall,
    status: ToolCallStatus,
    resume: Option<ToolCallResume>,
) -> Result<bool, AgentLoopError> {
    let current_state = tool_call_states_from_ctx(run_ctx).remove(&suspended_call.call_id);
    let Some(tool_state) = transition_tool_call_state(
        current_state,
        ToolCallStateSeed {
            call_id: &suspended_call.call_id,
            tool_name: &suspended_call.tool_name,
            arguments: &suspended_call.arguments,
            status: ToolCallStatus::Suspended,
            resume_token: Some(suspended_call.ticket.pending.id.clone()),
        },
        ToolCallStateTransition {
            status,
            resume_token: Some(suspended_call.ticket.pending.id.clone()),
            resume,
            updated_at: current_unix_millis(),
        },
    ) else {
        return Ok(false);
    };

    let base_state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let patch = upsert_tool_call_state(&base_state, &suspended_call.call_id, tool_state)?;
    if patch.patch().is_empty() {
        return Ok(false);
    }
    run_ctx.add_thread_patch(patch);
    Ok(true)
}

#[cfg(feature = "permission")]
fn suspended_call_decision_state_actions(
    suspended_call: &SuspendedCall,
    response: &ToolCallDecision,
) -> Vec<AnyStateAction> {
    tirea_extension_permission::remembered_permission_state_action(suspended_call, response)
        .into_iter()
        .collect()
}

#[cfg(not(feature = "permission"))]
fn suspended_call_decision_state_actions(
    _suspended_call: &SuspendedCall,
    _response: &ToolCallDecision,
) -> Vec<AnyStateAction> {
    Vec::new()
}

fn apply_decision_state_actions(
    run_ctx: &mut RunContext,
    actions: Vec<AnyStateAction>,
) -> Result<bool, AgentLoopError> {
    if actions.is_empty() {
        return Ok(false);
    }

    let serialized_actions = actions
        .iter()
        .map(AnyStateAction::to_serialized_state_action)
        .collect::<Vec<_>>();
    run_ctx.add_serialized_state_actions(serialized_actions);

    let base_state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let patches = reduce_state_actions(
        actions,
        &base_state,
        "agent_loop:decision",
        &ScopeContext::run(),
    )
    .map_err(|e| {
        AgentLoopError::StateError(format!("failed to reduce decision state actions: {e}"))
    })?;

    let mut changed = false;
    for patch in patches {
        if patch.patch().is_empty() {
            continue;
        }
        changed = true;
        run_ctx.add_thread_patch(patch);
    }
    Ok(changed)
}

pub(super) fn resolve_suspended_call(
    run_ctx: &mut RunContext,
    response: &ToolCallDecision,
) -> Result<Option<DecisionReplayOutcome>, AgentLoopError> {
    let suspended_calls = suspended_calls_from_ctx(run_ctx);
    if suspended_calls.is_empty() {
        return Ok(None);
    }

    let suspended_call = suspended_calls
        .get(&response.target_id)
        .cloned()
        .or_else(|| {
            suspended_calls
                .values()
                .find(|call| {
                    call.ticket.suspension.id == response.target_id
                        || call.ticket.pending.id == response.target_id
                        || call.call_id == response.target_id
                })
                .cloned()
        });
    let Some(suspended_call) = suspended_call else {
        return Ok(None);
    };

    let _ = apply_decision_state_actions(
        run_ctx,
        suspended_call_decision_state_actions(&suspended_call, response),
    )?;

    let _ = upsert_tool_call_lifecycle_state(
        run_ctx,
        &suspended_call,
        ToolCallStatus::Resuming,
        Some(response.resume.clone()),
    )?;

    Ok(Some(DecisionReplayOutcome {
        events: Vec::new(),
        resolved_call_ids: vec![suspended_call.call_id],
    }))
}

pub(super) fn drain_decision_channel(
    run_ctx: &mut RunContext,
    decision_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
    pending_decisions: &mut VecDeque<ToolCallDecision>,
) -> Result<DecisionReplayOutcome, AgentLoopError> {
    let mut disconnected = false;
    if let Some(rx) = decision_rx.as_mut() {
        loop {
            match rx.try_recv() {
                Ok(response) => pending_decisions.push_back(response),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
    }
    if disconnected {
        *decision_rx = None;
    }

    if pending_decisions.is_empty() {
        return Ok(DecisionReplayOutcome {
            events: Vec::new(),
            resolved_call_ids: Vec::new(),
        });
    }

    let mut unresolved = VecDeque::new();
    let mut events = Vec::new();
    let mut resolved_call_ids = Vec::new();
    let mut seen = HashSet::new();

    while let Some(response) = pending_decisions.pop_front() {
        if let Some(outcome) = resolve_suspended_call(run_ctx, &response)? {
            for call_id in outcome.resolved_call_ids {
                if seen.insert(call_id.clone()) {
                    resolved_call_ids.push(call_id);
                }
            }
            events.extend(outcome.events);
        } else {
            unresolved.push_back(response);
        }
    }
    *pending_decisions = unresolved;

    Ok(DecisionReplayOutcome {
        events,
        resolved_call_ids,
    })
}

async fn replay_after_decisions(
    run_ctx: &mut RunContext,
    decisions_applied: bool,
    step_tool_provider: &Arc<dyn StepToolProvider>,
    agent: &dyn Agent,
    active_tool_descriptors: &mut Vec<crate::contracts::runtime::tool_call::ToolDescriptor>,
    pending_delta_commit: &PendingDeltaCommitContext<'_>,
) -> Result<Vec<AgentEvent>, AgentLoopError> {
    if !decisions_applied {
        return Ok(Vec::new());
    }

    let decision_tools = resolve_step_tool_snapshot(step_tool_provider, run_ctx).await?;
    *active_tool_descriptors = decision_tools.descriptors.clone();

    let decision_drain = drain_run_start_resume_replay(
        run_ctx,
        &decision_tools.tools,
        agent,
        active_tool_descriptors,
    )
    .await?;

    pending_delta_commit
        .commit(run_ctx, CheckpointReason::ToolResultsCommitted, false)
        .await?;

    Ok(decision_drain.events)
}

async fn apply_decisions_and_replay(
    run_ctx: &mut RunContext,
    decision_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
    pending_decisions: &mut VecDeque<ToolCallDecision>,
    step_tool_provider: &Arc<dyn StepToolProvider>,
    agent: &dyn Agent,
    active_tool_descriptors: &mut Vec<crate::contracts::runtime::tool_call::ToolDescriptor>,
    pending_delta_commit: &PendingDeltaCommitContext<'_>,
) -> Result<Vec<AgentEvent>, AgentLoopError> {
    Ok(drain_and_replay_decisions(
        run_ctx,
        decision_rx,
        pending_decisions,
        None,
        DecisionReplayInputs {
            step_tool_provider,
            agent,
            active_tool_descriptors,
        },
        pending_delta_commit,
    )
    .await?
    .events)
}

pub(super) struct DecisionReplayOutcome {
    events: Vec<AgentEvent>,
    resolved_call_ids: Vec<String>,
}

struct DecisionReplayInputs<'a> {
    step_tool_provider: &'a Arc<dyn StepToolProvider>,
    agent: &'a dyn Agent,
    active_tool_descriptors: &'a mut Vec<crate::contracts::runtime::tool_call::ToolDescriptor>,
}

async fn drain_and_replay_decisions(
    run_ctx: &mut RunContext,
    decision_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
    pending_decisions: &mut VecDeque<ToolCallDecision>,
    decision: Option<ToolCallDecision>,
    replay_inputs: DecisionReplayInputs<'_>,
    pending_delta_commit: &PendingDeltaCommitContext<'_>,
) -> Result<DecisionReplayOutcome, AgentLoopError> {
    if let Some(decision) = decision {
        pending_decisions.push_back(decision);
    }
    let decision_drain = drain_decision_channel(run_ctx, decision_rx, pending_decisions)?;
    let mut events = decision_drain.events;
    let replay_events = replay_after_decisions(
        run_ctx,
        !decision_drain.resolved_call_ids.is_empty(),
        replay_inputs.step_tool_provider,
        replay_inputs.agent,
        replay_inputs.active_tool_descriptors,
        pending_delta_commit,
    )
    .await?;
    events.extend(replay_events);

    Ok(DecisionReplayOutcome {
        events,
        resolved_call_ids: decision_drain.resolved_call_ids,
    })
}

async fn apply_decision_and_replay(
    run_ctx: &mut RunContext,
    response: ToolCallDecision,
    decision_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
    pending_decisions: &mut VecDeque<ToolCallDecision>,
    replay_inputs: DecisionReplayInputs<'_>,
    pending_delta_commit: &PendingDeltaCommitContext<'_>,
) -> Result<DecisionReplayOutcome, AgentLoopError> {
    drain_and_replay_decisions(
        run_ctx,
        decision_rx,
        pending_decisions,
        Some(response),
        replay_inputs,
        pending_delta_commit,
    )
    .await
}

async fn recv_decision(
    decision_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
) -> Option<ToolCallDecision> {
    let rx = decision_rx.as_mut()?;
    rx.recv().await
}

/// Run the full agent loop until completion, suspension, cancellation, or error.
///
/// This is the primary non-streaming entry point. Tools are passed directly
/// and used as the default tool set unless the agent's step_tool_provider is set
/// (for dynamic per-step tool resolution).
pub async fn run_loop(
    agent: &dyn Agent,
    tools: HashMap<String, Arc<dyn Tool>>,
    run_ctx: RunContext,
    cancellation_token: Option<RunCancellationToken>,
    state_committer: Option<Arc<dyn StateCommitter>>,
    decision_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
) -> LoopOutcome {
    let run_identity = run_ctx.run_identity().clone();
    run_loop_with_context(
        agent,
        tools,
        run_ctx,
        run_identity,
        cancellation_token,
        state_committer,
        decision_rx,
    )
    .await
}

/// Run the full agent loop until completion, suspension, cancellation, or error.
///
/// This is the primary non-streaming entry point. Tools are passed directly
/// and used as the default tool set unless the agent's step_tool_provider is set
/// (for dynamic per-step tool resolution).
pub async fn run_loop_with_context(
    agent: &dyn Agent,
    tools: HashMap<String, Arc<dyn Tool>>,
    mut run_ctx: RunContext,
    run_identity: RunIdentity,
    cancellation_token: Option<RunCancellationToken>,
    state_committer: Option<Arc<dyn StateCommitter>>,
    mut decision_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
) -> LoopOutcome {
    let run_identity = ensure_run_identity(agent, &mut run_ctx, run_identity);
    let executor = llm_executor_for_run(agent);
    let tool_executor = agent.tool_executor();
    let mut run_state = LoopRunState::new();
    let mut pending_decisions = VecDeque::new();
    let run_cancellation_token = cancellation_token;
    let mut last_text = String::new();
    let mut continued_response_prefix = String::new();
    let step_tool_provider = step_tool_provider_for_run(agent, tools);
    let run_id = run_identity.run_id.clone();
    let baseline_suspended_call_ids = suspended_call_ids(&run_ctx);
    let pending_delta_commit =
        PendingDeltaCommitContext::new(&run_identity, state_committer.as_ref());
    let initial_step_tools = match resolve_step_tool_snapshot(&step_tool_provider, &run_ctx).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let msg = error.to_string();
            return build_loop_outcome(
                run_ctx,
                TerminationReason::Error(msg.clone()),
                None,
                &run_state,
                Some(outcome::LoopFailure::State(msg)),
            );
        }
    };
    let StepToolSnapshot {
        tools: initial_tools,
        descriptors: initial_descriptors,
    } = initial_step_tools;
    let mut active_tool_descriptors = initial_descriptors;

    macro_rules! terminate_run {
        ($termination:expr, $response:expr, $failure:expr) => {{
            let reason: TerminationReason = $termination;
            let (final_termination, final_response) =
                normalize_termination_for_suspended_calls(&run_ctx, reason, $response);
            if let Err(error) = persist_run_termination(
                &mut run_ctx,
                &final_termination,
                &active_tool_descriptors,
                agent,
                &run_identity,
                &pending_delta_commit,
                run_state.token_totals(),
            )
            .await
            {
                let msg = error.to_string();
                return build_loop_outcome(
                    run_ctx,
                    TerminationReason::Error(msg.clone()),
                    None,
                    &run_state,
                    Some(outcome::LoopFailure::State(msg)),
                );
            }
            return build_loop_outcome(
                run_ctx,
                final_termination,
                final_response,
                &run_state,
                $failure,
            );
        }};
    }

    // Phase: RunStart
    let (pending, actions) = match plugin_runtime::emit_phase_block(
        Phase::RunStart,
        &run_ctx,
        &active_tool_descriptors,
        agent,
        |_| {},
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            let msg = error.to_string();
            terminate_run!(
                TerminationReason::Error(msg.clone()),
                None,
                Some(outcome::LoopFailure::State(msg))
            );
        }
    };
    run_ctx.add_thread_patches(pending);
    run_ctx.add_serialized_state_actions(actions);
    if let Err(error) = commit_run_start_and_drain_replay(
        &mut run_ctx,
        &initial_tools,
        agent,
        &active_tool_descriptors,
        &pending_delta_commit,
    )
    .await
    {
        let msg = error.to_string();
        terminate_run!(
            TerminationReason::Error(msg.clone()),
            None,
            Some(outcome::LoopFailure::State(msg))
        );
    }
    let run_start_new_suspended = newly_suspended_call_ids(&run_ctx, &baseline_suspended_call_ids);
    if !run_start_new_suspended.is_empty() {
        terminate_run!(TerminationReason::Suspended, None, None);
    }
    loop {
        if let Err(error) = apply_decisions_and_replay(
            &mut run_ctx,
            &mut decision_rx,
            &mut pending_decisions,
            &step_tool_provider,
            agent,
            &mut active_tool_descriptors,
            &pending_delta_commit,
        )
        .await
        {
            let msg = error.to_string();
            terminate_run!(
                TerminationReason::Error(msg.clone()),
                None,
                Some(outcome::LoopFailure::State(msg))
            );
        }

        if is_run_cancelled(run_cancellation_token.as_ref()) {
            terminate_run!(TerminationReason::Cancelled, None, None);
        }

        let step_tools = match resolve_step_tool_snapshot(&step_tool_provider, &run_ctx).await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                let msg = e.to_string();
                terminate_run!(
                    TerminationReason::Error(msg.clone()),
                    None,
                    Some(outcome::LoopFailure::State(msg))
                );
            }
        };
        active_tool_descriptors = step_tools.descriptors.clone();

        let prepared = match prepare_step_execution(&run_ctx, &active_tool_descriptors, agent).await
        {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                terminate_run!(
                    TerminationReason::Error(msg.clone()),
                    None,
                    Some(outcome::LoopFailure::State(msg))
                );
            }
        };
        run_ctx.add_thread_patches(prepared.pending_patches);

        match prepared.run_action {
            RunAction::Continue => {}
            RunAction::Terminate(reason) => {
                let response = if matches!(reason, TerminationReason::BehaviorRequested) {
                    Some(last_text.clone())
                } else {
                    None
                };
                terminate_run!(reason, response, None);
            }
        }

        // Call LLM with unified retry + fallback model strategy.
        let messages = prepared.messages;
        let filtered_tools = prepared.filtered_tools;
        let request_transforms = prepared.request_transforms;
        let step_inference_override = prepared.inference_override;
        let chat_options =
            apply_inference_override(agent.chat_options(), step_inference_override.as_ref());
        let attempt_outcome = run_llm_with_retry_and_fallback(
            agent,
            run_cancellation_token.as_ref(),
            true,
            None,
            step_inference_override.as_ref(),
            "unknown llm error",
            |model| {
                let request = build_request_for_filtered_tools(
                    &messages,
                    &step_tools.tools,
                    &filtered_tools,
                    &request_transforms,
                );
                let executor = executor.clone();
                let chat_options = chat_options.clone();
                async move {
                    executor
                        .exec_chat_response(&model, request, chat_options.as_ref())
                        .await
                }
            },
        )
        .await;

        let response = match attempt_outcome {
            LlmAttemptOutcome::Success {
                value, attempts, ..
            } => {
                run_state.record_llm_attempts(attempts);
                value
            }
            LlmAttemptOutcome::Cancelled => {
                append_cancellation_user_message(&mut run_ctx, CancellationStage::Inference);
                terminate_run!(TerminationReason::Cancelled, None, None);
            }
            LlmAttemptOutcome::Exhausted {
                last_error,
                last_error_class,
                attempts,
            } => {
                run_state.record_llm_attempts(attempts);
                if let Err(phase_error) = apply_llm_error_cleanup(
                    &mut run_ctx,
                    &active_tool_descriptors,
                    agent,
                    "llm_exec_error",
                    last_error.clone(),
                    last_error_class,
                )
                .await
                {
                    let msg = phase_error.to_string();
                    terminate_run!(
                        TerminationReason::Error(msg.clone()),
                        None,
                        Some(outcome::LoopFailure::State(msg))
                    );
                }
                terminate_run!(
                    TerminationReason::Error(last_error.clone()),
                    None,
                    Some(outcome::LoopFailure::Llm(last_error))
                );
            }
        };

        let result = stream_result_from_chat_response(&response);
        run_state.update_from_response(&result);
        last_text = stitch_response_text(&continued_response_prefix, &result.text);

        // Add assistant message
        let assistant_msg_id = gen_message_id();
        let step_meta = step_metadata(Some(run_id.clone()), run_state.completed_steps as u32);
        let post_inference_action = match complete_step_after_inference(
            &mut run_ctx,
            &result,
            step_meta.clone(),
            Some(assistant_msg_id.clone()),
            &active_tool_descriptors,
            agent,
        )
        .await
        {
            Ok(action) => action,
            Err(e) => {
                let msg = e.to_string();
                terminate_run!(
                    TerminationReason::Error(msg.clone()),
                    None,
                    Some(outcome::LoopFailure::State(msg))
                );
            }
        };
        if let Err(error) = pending_delta_commit
            .commit(
                &mut run_ctx,
                CheckpointReason::AssistantTurnCommitted,
                false,
            )
            .await
        {
            let msg = error.to_string();
            terminate_run!(
                TerminationReason::Error(msg.clone()),
                None,
                Some(outcome::LoopFailure::State(msg))
            );
        }

        mark_step_completed(&mut run_state);

        // Truncation recovery: if the model hit max_tokens with no tool
        // calls, inject a continuation prompt and re-enter inference.
        if truncation_recovery::should_retry(&result, &mut run_state) {
            extend_response_prefix(&mut continued_response_prefix, &result.text);
            let prompt = truncation_recovery::continuation_message();
            run_ctx.add_message(Arc::new(prompt));
            continue;
        }
        continued_response_prefix.clear();

        let post_inference_termination = match &post_inference_action {
            RunAction::Terminate(reason) => Some(reason.clone()),
            _ => None,
        };

        // Only `Stopped` termination is deferred past tool execution so the
        // current round's tools complete (e.g. MaxRounds lets tools finish).
        // All other reasons terminate immediately before tool execution.
        if let Some(reason) = &post_inference_termination {
            if !matches!(reason, TerminationReason::Stopped(_)) {
                terminate_run!(reason.clone(), Some(last_text.clone()), None);
            }
        }

        if !result.needs_tools() {
            run_state.record_step_without_tools();
            let reason = post_inference_termination.unwrap_or(TerminationReason::NaturalEnd);
            terminate_run!(reason, Some(last_text.clone()), None);
        }

        // Execute tools with phase hooks using configured execution strategy.
        let tool_context = match prepare_tool_execution_context(&run_ctx) {
            Ok(ctx) => ctx,
            Err(e) => {
                let msg = e.to_string();
                terminate_run!(
                    TerminationReason::Error(msg.clone()),
                    None,
                    Some(outcome::LoopFailure::State(msg))
                );
            }
        };
        let thread_messages_for_tools = run_ctx.messages().to_vec();
        let thread_version_for_tools = run_ctx.version();

        let tool_exec_future = tool_executor.execute(ToolExecutionRequest {
            tools: &step_tools.tools,
            calls: &result.tool_calls,
            state: &tool_context.state,
            tool_descriptors: &active_tool_descriptors,
            agent_behavior: Some(agent.behavior()),
            activity_manager: tirea_contract::runtime::activity::NoOpActivityManager::arc(),
            run_policy: &tool_context.run_policy,
            run_identity: tool_context.run_identity.clone(),
            caller_context: tool_context.caller_context.clone(),
            thread_id: run_ctx.thread_id(),
            thread_messages: &thread_messages_for_tools,
            state_version: thread_version_for_tools,
            cancellation_token: run_cancellation_token.as_ref(),
        });
        let results = tool_exec_future.await.map_err(AgentLoopError::from);

        let results = match results {
            Ok(r) => r,
            Err(AgentLoopError::Cancelled) => {
                append_cancellation_user_message(&mut run_ctx, CancellationStage::ToolExecution);
                terminate_run!(TerminationReason::Cancelled, None, None);
            }
            Err(e) => {
                let msg = e.to_string();
                terminate_run!(
                    TerminationReason::Error(msg.clone()),
                    None,
                    Some(outcome::LoopFailure::State(msg))
                );
            }
        };

        if let Err(_e) = apply_tool_results_to_session(
            &mut run_ctx,
            &results,
            Some(step_meta),
            tool_executor.requires_parallel_patch_conflict_check(),
        ) {
            // On error, we can't easily rollback RunContext, so just terminate
            let msg = _e.to_string();
            terminate_run!(
                TerminationReason::Error(msg.clone()),
                None,
                Some(outcome::LoopFailure::State(msg))
            );
        }
        if let Err(error) = pending_delta_commit
            .commit(&mut run_ctx, CheckpointReason::ToolResultsCommitted, false)
            .await
        {
            let msg = error.to_string();
            terminate_run!(
                TerminationReason::Error(msg.clone()),
                None,
                Some(outcome::LoopFailure::State(msg))
            );
        }

        if let Err(error) = apply_decisions_and_replay(
            &mut run_ctx,
            &mut decision_rx,
            &mut pending_decisions,
            &step_tool_provider,
            agent,
            &mut active_tool_descriptors,
            &pending_delta_commit,
        )
        .await
        {
            let msg = error.to_string();
            terminate_run!(
                TerminationReason::Error(msg.clone()),
                None,
                Some(outcome::LoopFailure::State(msg))
            );
        }

        // If ALL tools are suspended (no completed results), terminate immediately.
        if has_suspended_calls(&run_ctx) {
            let has_completed = results
                .iter()
                .any(|r| !matches!(r.outcome, crate::contracts::ToolCallOutcome::Suspended));
            if !has_completed {
                terminate_run!(TerminationReason::Suspended, None, None);
            }
        }

        // Deferred post-inference termination: tools from the current round
        // have completed; stop the loop before the next inference.
        if let Some(reason) = post_inference_termination {
            terminate_run!(reason, Some(last_text.clone()), None);
        }

        // Track tool-step metrics for loop stats and plugin consumers.
        let error_count = results
            .iter()
            .filter(|r| r.execution.result.is_error())
            .count();
        run_state.record_tool_step(&result.tool_calls, error_count);
    }
}

/// Run the agent loop with streaming output.
///
/// Returns a stream of AgentEvent for real-time updates. Tools are passed
/// directly and used as the default tool set unless the agent's step_tool_provider
/// is set (for dynamic per-step tool resolution).
pub fn run_loop_stream(
    agent: Arc<dyn Agent>,
    tools: HashMap<String, Arc<dyn Tool>>,
    run_ctx: RunContext,
    cancellation_token: Option<RunCancellationToken>,
    state_committer: Option<Arc<dyn StateCommitter>>,
    decision_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
    let run_identity = run_ctx.run_identity().clone();
    run_loop_stream_with_context(
        agent,
        tools,
        run_ctx,
        run_identity,
        cancellation_token,
        state_committer,
        decision_rx,
    )
}

pub fn run_loop_stream_with_context(
    agent: Arc<dyn Agent>,
    tools: HashMap<String, Arc<dyn Tool>>,
    mut run_ctx: RunContext,
    run_identity: RunIdentity,
    cancellation_token: Option<RunCancellationToken>,
    state_committer: Option<Arc<dyn StateCommitter>>,
    decision_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
    let run_identity = ensure_run_identity(agent.as_ref(), &mut run_ctx, run_identity);
    stream_runner::run_stream(
        agent,
        tools,
        run_ctx,
        run_identity,
        cancellation_token,
        state_committer,
        decision_rx,
    )
}

#[cfg(test)]
mod tests;
