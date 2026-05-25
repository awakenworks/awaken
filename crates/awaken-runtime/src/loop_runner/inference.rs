//! LLM inference execution.
use super::{AgentLoopError, now_ms};
use crate::cancellation::CancellationToken;
use crate::registry::ResolvedAgent;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::executor::{
    InFlightTool, InferenceExecutionError, InferenceRequest, InterruptCause, InterruptSnapshot,
    LlmStreamEvent, RecoveryPlan,
};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::message::{Message, ToolCall};
use awaken_contract::contract::stream_checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use futures::StreamExt;
use std::time::Duration;

use super::stream_policy::{idle_timeout_for, stream_retry_backoff, stream_retry_policy_for};
/// Identifies a run for stream-checkpoint purposes. Passed into
/// `execute_streaming` by the caller that actually knows the run
/// identity (the loop runner's step driver); tests that don't care
/// about cross-process resume pass `None`.
pub(super) struct CheckpointHandle<'a> {
    pub store: &'a dyn StreamCheckpointStore,
    pub run_id: &'a str,
    pub thread_id: &'a str,
}
/// Minimum delta interval between checkpoint flushes. Prevents one
/// flush per tokenized chunk at the cost of losing up to this many
/// deltas on a hard crash.
const CHECKPOINT_FLUSH_DELTAS: usize = 4;
/// Write the current accumulator state to the checkpoint store. Logs and
/// suppresses backend errors — a failing checkpoint store must never
/// disrupt the in-flight stream.
async fn flush_checkpoint(
    acc: &StreamingAccumulator,
    upstream_model: &str,
    handle: &CheckpointHandle<'_>,
) {
    let snapshot = acc.interrupt_snapshot();
    let checkpoint = StreamCheckpoint {
        run_id: handle.run_id.to_string(),
        thread_id: handle.thread_id.to_string(),
        upstream_model: upstream_model.to_string(),
        partial_text: snapshot.text.clone().unwrap_or_default(),
        completed_tool_calls: snapshot.completed_tool_calls,
        in_flight_tool: snapshot.in_flight_tool,
        updated_at_ms: now_ms(),
    };
    if let Err(e) = handle.store.put(checkpoint).await {
        tracing::warn!(
            run_id = %handle.run_id,
            error = %e,
            "stream checkpoint flush failed — continuing without persistence",
        );
    }
}
/// Continuation prompt injected after a mid-stream interruption to nudge
/// the model to pick up where its partial response left off. Intentionally
/// short — long prompts dilute the assistant prefix context.
const CONTINUATION_PROMPT: &str =
    "Your previous response was interrupted mid-stream. Continue from where you left off.";
/// Execute LLM inference with streaming, emitting delta events via sink.
///
/// Consumes the token stream from `execute_stream()`, forwards deltas to
/// sink, and collects the final `StreamResult`. Drives the in-process R1-R4
/// retry loop on mid-stream interruption and, when a `CheckpointHandle` is
/// supplied, persists / restores accumulator state for cross-process resume.
///
/// Supports mid-stream cancellation: if the `CancellationToken` is signalled
/// while waiting for the next token, the stream is dropped and the partially
/// accumulated result is returned with `StopReason::EndTurn` (graceful
/// cancel — no error).
///
/// Returns `(stream_result, cancelled_tool_hint)`. `cancelled_tool_hint`
/// is `Some` only when an in-flight parallel tool was dropped during
/// recovery; the loop runner uses it to inject a user-visible note in
/// the next turn.
pub(super) async fn execute_streaming(
    agent: &ResolvedAgent,
    mut request: InferenceRequest,
    sink: &dyn EventSink,
    cancellation_token: Option<&CancellationToken>,
    total_input_tokens: &mut u64,
    total_output_tokens: &mut u64,
    checkpoint: Option<CheckpointHandle<'_>>,
) -> Result<(StreamResult, Option<InFlightTool>), AgentLoopError> {
    super::logical_inference::ensure_logical_inference_id(&mut request);
    let policy = stream_retry_policy_for(agent);
    let idle_timeout = idle_timeout_for(&request, &policy);
    let max_retries = policy.max_stream_retries;
    let mut attempt: u32 = 0;
    // Cross-process resume: if a checkpoint exists for this run id,
    // synthesize a `DriveOutcome::Interrupted` and feed it through the
    // same recovery dispatch the in-process retry loop uses. This means
    // crash-resume reuses every R1-R4 code path verbatim — there is no
    // separate "restore" code path to keep in sync.
    let mut pending_resume: Option<DriveOutcome> = None;
    if let Some(handle) = checkpoint.as_ref()
        && let Some(saved) = read_checkpoint(handle).await
    {
        let snapshot = InterruptSnapshot::from_partials(
            (!saved.partial_text.is_empty()).then(|| saved.partial_text.clone()),
            saved
                .completed_tool_calls
                .into_iter()
                .map(|c| {
                    (
                        c.id,
                        c.name,
                        serde_json::to_string(&c.arguments).unwrap_or_default(),
                    )
                })
                .chain(
                    saved
                        .in_flight_tool
                        .into_iter()
                        .map(|p| (p.id, p.name, p.partial_args)),
                ),
            saved.partial_text.len(),
        );
        pending_resume = Some(DriveOutcome::Interrupted {
            cause: InterruptCause::ResumedFromCheckpoint,
            snapshot,
        });
    }
    loop {
        // Consume the synthetic resume outcome on the first iteration,
        // then fall through to driving real streams on subsequent ones.
        let outcome = match pending_resume.take() {
            Some(o) => o,
            None => {
                drive_one_stream(
                    agent,
                    request.clone(),
                    sink,
                    cancellation_token,
                    total_input_tokens,
                    total_output_tokens,
                    idle_timeout,
                    checkpoint.as_ref(),
                )
                .await
            }
        };
        match outcome {
            DriveOutcome::Completed(result) | DriveOutcome::Cancelled(result) => {
                // On clean completion delete the checkpoint — it has been
                // fully consumed and should not leak across runs.
                if let Some(handle) = checkpoint.as_ref() {
                    let _ = handle.store.delete(handle.run_id).await;
                }
                return Ok((result, None));
            }
            DriveOutcome::Error(err) => return Err(err),
            DriveOutcome::Interrupted { cause, snapshot } => {
                // Resume-from-checkpoint never counts against the retry
                // budget — it is a "free" first attempt that mutates the
                // request, then proceeds to drive a real stream.
                let counts_against_budget = !matches!(cause, InterruptCause::ResumedFromCheckpoint);
                if counts_against_budget && attempt >= max_retries {
                    tracing::warn!(
                        attempts = attempt,
                        cause = %cause,
                        bytes_received = snapshot.bytes_received,
                        "stream retry budget exhausted; surfacing StreamInterrupted",
                    );
                    return Err(AgentLoopError::from(
                        InferenceExecutionError::StreamInterrupted {
                            cause,
                            snapshot: Box::new(snapshot),
                        },
                    ));
                }
                match apply_recovery_plan(&mut request, sink, &cause, &snapshot).await {
                    RecoveryOutcome::SynthesizedToolUse { result, hint } => {
                        if let Some(handle) = checkpoint.as_ref() {
                            let _ = handle.store.delete(handle.run_id).await;
                        }
                        return Ok((result, hint));
                    }
                    RecoveryOutcome::RetryAfterPlan => {
                        if counts_against_budget {
                            let delay = stream_retry_backoff(&cause, attempt, &policy);
                            if !delay.is_zero() {
                                if let Some(token) = cancellation_token {
                                    tokio::select! {
                                        biased;
                                        _ = token.cancelled() => {
                                            return Err(AgentLoopError::from(
                                                InferenceExecutionError::Cancelled,
                                            ));
                                        }
                                        _ = tokio::time::sleep(delay) => {}
                                    }
                                } else {
                                    tokio::time::sleep(delay).await;
                                }
                            }
                            attempt += 1;
                        }
                        continue;
                    }
                }
            }
        }
    }
}
/// Best-effort checkpoint read. Logs and swallows backend errors so a
/// failing store can never block forward progress.
async fn read_checkpoint(handle: &CheckpointHandle<'_>) -> Option<StreamCheckpoint> {
    match handle.store.get(handle.run_id).await {
        Ok(Some(saved)) => {
            tracing::info!(
                run_id = %handle.run_id,
                partial_text_len = saved.partial_text.len(),
                completed_tools = saved.completed_tool_calls.len(),
                has_in_flight = saved.in_flight_tool.is_some(),
                "restoring stream checkpoint"
            );
            Some(saved)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                run_id = %handle.run_id,
                error = %e,
                "checkpoint read failed; continuing without restore"
            );
            None
        }
    }
}
/// Result of driving a single stream attempt to completion.
enum DriveOutcome {
    /// Stream finished naturally.
    Completed(StreamResult),
    /// Cancellation token fired; partial state returned with
    /// `StopReason::EndTurn` (graceful).
    Cancelled(StreamResult),
    /// Mid-stream transport/idle failure; caller decides whether to retry.
    Interrupted {
        cause: InterruptCause,
        snapshot: InterruptSnapshot,
    },
    /// Non-recoverable runtime error (stream open failed with permanent
    /// error, sink emit failed, etc.).
    Error(AgentLoopError),
}
enum RecoveryOutcome {
    /// R2 path: return the synthesized tool-use stream result directly to
    /// the caller without another inference round-trip. The in-flight
    /// tool (if any) is surfaced as a hint so the caller can append a
    /// note to the next turn's user message.
    SynthesizedToolUse {
        result: StreamResult,
        hint: Option<InFlightTool>,
    },
    /// R1/R3/R4 paths: `request` has been mutated (R1/R3) or left as-is
    /// (R4); control flow loops back and opens a fresh stream.
    RetryAfterPlan,
}
async fn apply_recovery_plan(
    request: &mut InferenceRequest,
    sink: &dyn EventSink,
    cause: &InterruptCause,
    snapshot: &InterruptSnapshot,
) -> RecoveryOutcome {
    match snapshot.plan() {
        RecoveryPlan::ContinueText { assistant_prefix } => {
            push_continuation(request, assistant_prefix);
            RecoveryOutcome::RetryAfterPlan
        }
        RecoveryPlan::SynthesizeToolUse {
            completed,
            cancelled_tool_hint,
        } => {
            if let Some(hint) = &cancelled_tool_hint {
                sink.emit(AgentEvent::ToolCallCancel {
                    id: hint.id.clone(),
                    name: hint.name.clone(),
                    reason: cause.to_string(),
                })
                .await;
            }
            // Emit ToolCallReady for each completed tool so downstream
            // consumers (UI, telemetry) see the same sequence they would
            // have on a normal `StopReason::ToolUse` termination.
            for call in &completed {
                sink.emit(AgentEvent::ToolCallReady {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .await;
            }
            let content = match snapshot.text.as_ref() {
                Some(t) if !t.is_empty() => vec![ContentBlock::text(t.clone())],
                _ => Vec::new(),
            };
            RecoveryOutcome::SynthesizedToolUse {
                result: StreamResult {
                    content,
                    tool_calls: completed,
                    usage: None,
                    stop_reason: Some(StopReason::ToolUse),
                    has_incomplete_tool_calls: false,
                },
                hint: cancelled_tool_hint,
            }
        }
        RecoveryPlan::TruncateBeforeTool {
            assistant_prefix,
            cancelled_tool_id,
            cancelled_tool_name,
        } => {
            sink.emit(AgentEvent::ToolCallCancel {
                id: cancelled_tool_id,
                name: cancelled_tool_name,
                reason: cause.to_string(),
            })
            .await;
            push_continuation(request, assistant_prefix);
            RecoveryOutcome::RetryAfterPlan
        }
        RecoveryPlan::WholeRestart => {
            sink.emit(AgentEvent::StreamReset {
                reason: cause.to_string(),
            })
            .await;
            RecoveryOutcome::RetryAfterPlan
        }
    }
}

fn push_continuation(request: &mut InferenceRequest, assistant_prefix: String) {
    if !assistant_prefix.is_empty() {
        request.messages.push(Message::assistant(assistant_prefix));
    }
    request.messages.push(Message::user(CONTINUATION_PROMPT));
}

/// Drive a single `execute_stream` call to completion, returning one of
/// three outcomes. The mid-stream error-to-`InterruptCause` classification
/// lives here.
// internal stream-driving helper; argument grouping into a struct would add wrapper noise without reducing call complexity
#[allow(clippy::too_many_arguments)]
async fn drive_one_stream(
    agent: &ResolvedAgent,
    request: InferenceRequest,
    sink: &dyn EventSink,
    cancellation_token: Option<&CancellationToken>,
    total_input_tokens: &mut u64,
    total_output_tokens: &mut u64,
    idle_timeout: Duration,
    checkpoint: Option<&CheckpointHandle<'_>>,
) -> DriveOutcome {
    let upstream_model = request.upstream_model.clone();
    let mut token_stream = match agent.llm_executor.execute_stream(request.clone()).await {
        Ok(s) => s,
        Err(err) => {
            // `err` here comes from the executor (including `RetryingExecutor`)
            // and has already exhausted its own retries. Surface it.
            return DriveOutcome::Error(AgentLoopError::from(err));
        }
    };

    let mut acc = StreamingAccumulator::default();
    let mut deltas_since_last_flush: usize = 0;

    loop {
        let next_fut = async { tokio::time::timeout(idle_timeout, token_stream.next()).await };

        let event = if let Some(token) = cancellation_token {
            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    acc.cancelled = true;
                    break;
                }
                r = next_fut => r,
            }
        } else {
            next_fut.await
        };

        let poll = match event {
            Ok(p) => p,
            Err(_) => {
                // Idle stall — no delta in `idle_timeout`. Flush
                // whatever we had before surrendering to recovery.
                if let Some(handle) = checkpoint {
                    flush_checkpoint(&acc, &upstream_model, handle).await;
                }
                let snapshot = acc.interrupt_snapshot();
                let err = InferenceExecutionError::StreamInterrupted {
                    cause: InterruptCause::IdleStall,
                    snapshot: Box::new(snapshot.clone()),
                };
                agent.llm_executor.record_stream_failure(&request, &err);
                return DriveOutcome::Interrupted {
                    cause: InterruptCause::IdleStall,
                    snapshot,
                };
            }
        };

        let Some(event_result) = poll else {
            break; // stream ended cleanly
        };

        let event = match event_result {
            Ok(ev) => ev,
            Err(err) => {
                // Mid-stream transport error. Flush accumulator state
                // for cross-process resume before surfacing to the
                // in-process recovery loop.
                if let Some(handle) = checkpoint {
                    flush_checkpoint(&acc, &upstream_model, handle).await;
                }
                let snapshot = acc.interrupt_snapshot();
                match classify_mid_stream(&err) {
                    Some(cause) => {
                        tracing::debug!(
                            cause = %cause,
                            bytes_received = snapshot.bytes_received,
                            "mid-stream error captured, entering recovery"
                        );
                        let observed = InferenceExecutionError::StreamInterrupted {
                            cause: cause.clone(),
                            snapshot: Box::new(snapshot.clone()),
                        };
                        agent
                            .llm_executor
                            .record_stream_failure(&request, &observed);
                        return DriveOutcome::Interrupted { cause, snapshot };
                    }
                    None => {
                        agent.llm_executor.record_stream_failure(&request, &err);
                        return DriveOutcome::Error(AgentLoopError::from(err));
                    }
                }
            }
        };

        let mut saw_delta = false;
        match event {
            LlmStreamEvent::TextDelta(delta) => {
                saw_delta = true;
                acc.current_text.push_str(&delta);
                sink.emit(AgentEvent::TextDelta { delta }).await;
            }
            LlmStreamEvent::ReasoningDelta(delta) => {
                sink.emit(AgentEvent::ReasoningDelta { delta }).await;
            }
            LlmStreamEvent::ToolCallStart { id, name } => {
                saw_delta = true;
                sink.emit(AgentEvent::ToolCallStart {
                    id: id.clone(),
                    name: name.clone(),
                })
                .await;
                acc.tool_names.insert(id.clone(), name);
                acc.current_tool_args.insert(id.clone(), String::new());
                acc.tool_order.push(id);
            }
            LlmStreamEvent::ToolCallDelta { id, args_delta } => {
                saw_delta = true;
                if let Some(buf) = acc.current_tool_args.get_mut(&id) {
                    buf.push_str(&args_delta);
                }
                sink.emit(AgentEvent::ToolCallDelta { id, args_delta })
                    .await;
            }
            LlmStreamEvent::ContentBlockStop => {
                if !acc.current_text.is_empty() {
                    acc.content_blocks
                        .push(ContentBlock::text(std::mem::take(&mut acc.current_text)));
                }
            }
            LlmStreamEvent::Usage(u) => {
                if let Some(v) = u.prompt_tokens {
                    *total_input_tokens = total_input_tokens.saturating_add(v.max(0) as u64);
                }
                if let Some(v) = u.completion_tokens {
                    *total_output_tokens = total_output_tokens.saturating_add(v.max(0) as u64);
                }
                acc.usage = Some(u);
            }
            LlmStreamEvent::Stop(reason) => {
                acc.stop_reason = Some(reason);
            }
        }

        if saw_delta {
            deltas_since_last_flush += 1;
            if deltas_since_last_flush >= CHECKPOINT_FLUSH_DELTAS {
                deltas_since_last_flush = 0;
                if let Some(handle) = checkpoint {
                    flush_checkpoint(&acc, &upstream_model, handle).await;
                }
            }
        }
    }

    // Stream drained cleanly (or cancelled): finalize.
    let result = acc.finalize(sink).await;
    if acc.cancelled {
        DriveOutcome::Cancelled(result)
    } else {
        agent.llm_executor.record_stream_success(&request);
        DriveOutcome::Completed(result)
    }
}

#[derive(Default)]
struct StreamingAccumulator {
    content_blocks: Vec<ContentBlock>,
    usage: Option<TokenUsage>,
    stop_reason: Option<StopReason>,
    current_text: String,
    current_tool_args: std::collections::HashMap<String, String>,
    tool_names: std::collections::HashMap<String, String>,
    tool_order: Vec<String>,
    bytes_received: usize,
    cancelled: bool,
}

impl StreamingAccumulator {
    /// Build an [`InterruptSnapshot`] reflecting the current accumulator
    /// state. Delegates partials → snapshot translation to the contract
    /// helper so the two stream collectors in this crate stay in lockstep.
    fn interrupt_snapshot(&self) -> InterruptSnapshot {
        let text = if self.current_text.is_empty() {
            self.content_blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } if !text.is_empty() => Some(text.clone()),
                    _ => None,
                })
                .reduce(|a, b| a + &b)
        } else {
            Some(self.current_text.clone())
        };
        let partials = self.tool_order.iter().map(|id| {
            (
                id.clone(),
                self.tool_names.get(id).cloned().unwrap_or_default(),
                self.current_tool_args.get(id).cloned().unwrap_or_default(),
            )
        });
        InterruptSnapshot::from_partials(text, partials, self.bytes_received)
    }

    async fn finalize(&mut self, sink: &dyn EventSink) -> StreamResult {
        // Flush remaining text into a content block.
        if !self.current_text.is_empty() {
            self.content_blocks
                .push(ContentBlock::text(std::mem::take(&mut self.current_text)));
        }

        let mut tool_calls = Vec::new();
        let mut has_incomplete_tool_calls = false;

        if !self.cancelled {
            for id in &self.tool_order {
                let args_json = self.current_tool_args.get(id).cloned().unwrap_or_default();
                let name = self.tool_names.get(id).cloned().unwrap_or_default();
                let arguments = serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
                if arguments.is_null() && !args_json.is_empty() {
                    has_incomplete_tool_calls = true;
                    continue;
                }
                tool_calls.push(ToolCall::new(id.clone(), name.clone(), arguments.clone()));
                sink.emit(AgentEvent::ToolCallReady {
                    id: id.clone(),
                    name,
                    arguments,
                })
                .await;
            }
        }

        StreamResult {
            content: std::mem::take(&mut self.content_blocks),
            tool_calls,
            usage: self.usage.take(),
            stop_reason: if self.cancelled {
                Some(StopReason::EndTurn)
            } else {
                self.stop_reason.take()
            },
            has_incomplete_tool_calls,
        }
    }
}

/// Classify a mid-stream error into an `InterruptCause`. Returns `None`
/// when the error is of a kind that should NOT enter the recovery loop
/// (e.g. `ContextOverflow`, `Unauthorized`, `Cancelled`) — those propagate
/// as a regular `Error` outcome.
fn classify_mid_stream(err: &InferenceExecutionError) -> Option<InterruptCause> {
    match err {
        // Recoverable — transport-ish failures.
        InferenceExecutionError::Provider(msg) | InferenceExecutionError::Timeout(msg) => {
            Some(interpret_transport_message(msg))
        }
        InferenceExecutionError::RateLimited { message, .. }
        | InferenceExecutionError::Overloaded { message, .. } => {
            Some(interpret_transport_message(message))
        }

        // Already-classified stream interruption — preserve cause.
        InferenceExecutionError::StreamInterrupted { cause, .. } => Some(cause.clone()),

        // Permanent / lifecycle — surface to caller, not a mid-stream retry.
        _ => None,
    }
}

/// Heuristic substring match to distinguish HTTP/2 GOAWAY — which is a
/// graceful server-initiated disconnect — from a raw TCP reset. The
/// difference matters for telemetry (GOAWAY is benign, TCP reset is not)
/// and for future policy (GOAWAY often warrants immediate fallback to a
/// different endpoint rather than a naive retry).
///
/// `genai` surfaces these through error messages whose contents are
/// provider- / reqwest-dependent, so string matching is the pragmatic
/// contract. Keep patterns case-insensitive.
fn interpret_transport_message(msg: &str) -> InterruptCause {
    let lower = msg.to_lowercase();
    if lower.contains("goaway")
        || lower.contains("go_away")
        || lower.contains("http/2 going away")
        || lower.contains("connection: close")
    {
        InterruptCause::GoAway
    } else if lower.contains("connection reset") || lower.contains("econnreset") {
        InterruptCause::ConnectionReset
    } else if lower.starts_with("502")
        || lower.starts_with("503")
        || lower.contains("502 bad gateway")
        || lower.contains("503 service unavailable")
    {
        // Gateway-level 5xx that reaches us after the stream opened is
        // treated as a mid-stream provider fault. The actual status
        // code is typically available in `msg`, but for
        // classification we only need the shape.
        InterruptCause::Provider5xxMidStream(503)
    } else {
        InterruptCause::ConnectionReset
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::cancellation::CancellationToken;
    use crate::engine::retry::LlmRetryPolicy;
    use crate::registry::ResolvedAgent;
    use async_trait::async_trait;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::event::AgentEvent;
    use awaken_contract::contract::event_sink::VecEventSink;
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, InferenceStream, LlmStreamEvent,
    };
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::contract::message::Message;

    // -- Scripted LLM executor used by every test in this module --

    /// Mock LLM executor that yields scripted events keyed by attempt
    /// number. On the Nth call to `execute_stream`, yields
    /// `scripts[min(N, scripts.len() - 1)]` so a single-element script
    /// repeats forever (legacy use case) and a multi-element script
    /// drives different per-attempt behavior (R1-R4 retry tests).
    struct ScriptedPerAttemptExecutor {
        scripts: Vec<Vec<Result<LlmStreamEvent, InferenceExecutionError>>>,
        attempt: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedPerAttemptExecutor {
        fn new(scripts: Vec<Vec<Result<LlmStreamEvent, InferenceExecutionError>>>) -> Self {
            assert!(!scripts.is_empty(), "need at least one attempt script");
            Self {
                scripts,
                attempt: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn attempts(&self) -> usize {
            self.attempt.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl awaken_contract::contract::executor::LlmExecutor for ScriptedPerAttemptExecutor {
        async fn execute(
            &self,
            _r: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Err(InferenceExecutionError::Provider("unused".into()))
        }

        fn execute_stream(
            &self,
            _request: InferenceRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<InferenceStream, InferenceExecutionError>>
                    + Send
                    + '_,
            >,
        > {
            let n = self
                .attempt
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let idx = n.min(self.scripts.len() - 1);
            let events = self.scripts[idx].clone();
            Box::pin(async move { Ok(Box::pin(futures::stream::iter(events)) as InferenceStream) })
        }

        fn name(&self) -> &str {
            "scripted-per-attempt"
        }
    }

    /// Build an agent backed by a single-script executor (the script
    /// repeats on every attempt).
    fn make_agent(events: Vec<Result<LlmStreamEvent, InferenceExecutionError>>) -> ResolvedAgent {
        agent_with(Arc::new(ScriptedPerAttemptExecutor::new(vec![events])))
    }

    fn agent_with(exec: Arc<ScriptedPerAttemptExecutor>) -> ResolvedAgent {
        ResolvedAgent::new("test-agent", "test-model", "system prompt", exec)
    }

    fn make_request() -> InferenceRequest {
        InferenceRequest {
            upstream_model: "test-model".into(),
            routing_key: None,
            messages: vec![Message::user("hello")],
            tools: vec![],
            system: vec![],
            overrides: None,
            enable_prompt_cache: false,
        }
    }
    // -- Text streaming --

    #[tokio::test]
    async fn collects_text_deltas_into_content_blocks() {
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::TextDelta("Hello ".into())),
            Ok(LlmStreamEvent::TextDelta("world!".into())),
            Ok(LlmStreamEvent::ContentBlockStop),
            Ok(LlmStreamEvent::Stop(StopReason::EndTurn)),
        ]);
        let sink = VecEventSink::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;

        let (result, _hint) = execute_streaming(
            &agent,
            make_request(),
            &sink,
            None,
            &mut input_tokens,
            &mut output_tokens,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello world!"),
            other => panic!("expected Text block, got: {other:?}"),
        }
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
    }

    #[tokio::test]
    async fn emits_text_delta_events_to_sink() {
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::TextDelta("hi".into())),
            Ok(LlmStreamEvent::ContentBlockStop),
        ]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
            .await
            .unwrap();

        let events = sink.take();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::TextDelta { delta } if delta == "hi")),
            "expected TextDelta event in sink"
        );
    }

    // -- Token counting --

    #[tokio::test]
    async fn accumulates_token_usage() {
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::Usage(TokenUsage {
                prompt_tokens: Some(50),
                completion_tokens: Some(25),
                total_tokens: Some(75),
                ..Default::default()
            })),
            Ok(LlmStreamEvent::Stop(StopReason::EndTurn)),
        ]);
        let sink = VecEventSink::new();
        let mut input_tokens = 10u64;
        let mut output_tokens = 5u64;

        let (result, _hint) = execute_streaming(
            &agent,
            make_request(),
            &sink,
            None,
            &mut input_tokens,
            &mut output_tokens,
            None,
        )
        .await
        .unwrap();

        assert_eq!(input_tokens, 60); // 10 + 50
        assert_eq!(output_tokens, 30); // 5 + 25
        assert!(result.usage.is_some());
    }

    #[tokio::test]
    async fn token_counting_handles_negative_values() {
        let agent = make_agent(vec![Ok(LlmStreamEvent::Usage(TokenUsage {
            prompt_tokens: Some(-5),
            completion_tokens: Some(-10),
            ..Default::default()
        }))]);
        let sink = VecEventSink::new();
        let mut input_tokens = 100u64;
        let mut output_tokens = 50u64;

        execute_streaming(
            &agent,
            make_request(),
            &sink,
            None,
            &mut input_tokens,
            &mut output_tokens,
            None,
        )
        .await
        .unwrap();

        // negative values: max(0) = 0, so no change
        assert_eq!(input_tokens, 100);
        assert_eq!(output_tokens, 50);
    }

    // -- Tool call streaming --

    #[tokio::test]
    async fn collects_tool_calls_from_stream() {
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::ToolCallStart {
                id: "tc1".into(),
                name: "get_weather".into(),
            }),
            Ok(LlmStreamEvent::ToolCallDelta {
                id: "tc1".into(),
                args_delta: r#"{"city":"#.into(),
            }),
            Ok(LlmStreamEvent::ToolCallDelta {
                id: "tc1".into(),
                args_delta: r#""NYC"}"#.into(),
            }),
            Ok(LlmStreamEvent::ContentBlockStop),
            Ok(LlmStreamEvent::Stop(StopReason::ToolUse)),
        ]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) =
            execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
                .await
                .unwrap();

        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "get_weather");
        assert_eq!(result.tool_calls[0].arguments["city"], "NYC");
        assert!(!result.has_incomplete_tool_calls);
    }

    #[tokio::test]
    async fn emits_tool_call_start_and_delta_events() {
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::ToolCallStart {
                id: "tc1".into(),
                name: "search".into(),
            }),
            Ok(LlmStreamEvent::ToolCallDelta {
                id: "tc1".into(),
                args_delta: r#"{"q":"test"}"#.into(),
            }),
            Ok(LlmStreamEvent::Stop(StopReason::ToolUse)),
        ]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
            .await
            .unwrap();

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallStart { id, name } if id == "tc1" && name == "search"
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCallDelta { id, .. } if id == "tc1"))
        );
    }

    // -- Truncated / incomplete tool calls --

    #[tokio::test]
    async fn truncated_tool_call_json_marked_incomplete() {
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::ToolCallStart {
                id: "tc1".into(),
                name: "fetch".into(),
            }),
            Ok(LlmStreamEvent::ToolCallDelta {
                id: "tc1".into(),
                args_delta: r#"{"url":"https://exam"#.into(), // truncated JSON
            }),
            Ok(LlmStreamEvent::Stop(StopReason::MaxTokens)),
        ]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) =
            execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
                .await
                .unwrap();

        // Truncated tool call is skipped, but has_incomplete_tool_calls is flagged
        assert!(result.tool_calls.is_empty());
        assert!(result.has_incomplete_tool_calls);
    }

    // -- Multiple tool calls preserve order --

    #[tokio::test]
    async fn multiple_tool_calls_preserve_declaration_order() {
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::ToolCallStart {
                id: "tc1".into(),
                name: "tool_a".into(),
            }),
            Ok(LlmStreamEvent::ToolCallDelta {
                id: "tc1".into(),
                args_delta: "{}".into(),
            }),
            Ok(LlmStreamEvent::ToolCallStart {
                id: "tc2".into(),
                name: "tool_b".into(),
            }),
            Ok(LlmStreamEvent::ToolCallDelta {
                id: "tc2".into(),
                args_delta: r#"{"x":1}"#.into(),
            }),
            Ok(LlmStreamEvent::Stop(StopReason::ToolUse)),
        ]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) =
            execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
                .await
                .unwrap();

        assert_eq!(result.tool_calls.len(), 2);
        assert_eq!(result.tool_calls[0].name, "tool_a");
        assert_eq!(result.tool_calls[1].name, "tool_b");
    }

    // -- Cancellation --

    #[tokio::test]
    async fn cancellation_returns_end_turn_and_drops_tool_calls() {
        // Stream that blocks after text deltas -- we cancel before it completes
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::TextDelta("partial ".into())),
            Ok(LlmStreamEvent::ToolCallStart {
                id: "tc1".into(),
                name: "my_tool".into(),
            }),
            Ok(LlmStreamEvent::ToolCallDelta {
                id: "tc1".into(),
                args_delta: r#"{"key":"value"}"#.into(),
            }),
            // normally more events would follow
            Ok(LlmStreamEvent::Stop(StopReason::ToolUse)),
        ]);

        // Pre-cancel the token so the select branch picks it up
        let token = CancellationToken::new();
        token.cancel();

        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) = execute_streaming(
            &agent,
            make_request(),
            &sink,
            Some(&token),
            &mut it,
            &mut ot,
            None,
        )
        .await
        .unwrap();

        // Cancelled runs get EndTurn and no tool calls
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        assert!(result.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn no_cancellation_token_processes_full_stream() {
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::TextDelta("complete".into())),
            Ok(LlmStreamEvent::ContentBlockStop),
            Ok(LlmStreamEvent::Stop(StopReason::EndTurn)),
        ]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) =
            execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
                .await
                .unwrap();

        assert_eq!(result.content.len(), 1);
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
    }

    // -- Reasoning deltas --

    #[tokio::test]
    async fn reasoning_deltas_emitted_to_sink() {
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::ReasoningDelta("thinking...".into())),
            Ok(LlmStreamEvent::TextDelta("answer".into())),
            Ok(LlmStreamEvent::ContentBlockStop),
            Ok(LlmStreamEvent::Stop(StopReason::EndTurn)),
        ]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
            .await
            .unwrap();

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ReasoningDelta { delta } if delta == "thinking..."
        )));
    }

    // -- Empty stream --

    #[tokio::test]
    async fn empty_stream_returns_empty_result() {
        let agent = make_agent(vec![]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) =
            execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
                .await
                .unwrap();

        assert!(result.content.is_empty());
        assert!(result.tool_calls.is_empty());
        assert!(result.usage.is_none());
        assert!(result.stop_reason.is_none());
    }

    // -- Text flush on stream end without ContentBlockStop --

    #[tokio::test]
    async fn flushes_remaining_text_at_end_of_stream() {
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::TextDelta("no block stop".into())),
            Ok(LlmStreamEvent::Stop(StopReason::EndTurn)),
            // No ContentBlockStop emitted
        ]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) =
            execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
                .await
                .unwrap();

        // Text should still be flushed
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "no block stop"),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    // -- Stream error propagation --

    #[tokio::test]
    async fn stream_error_propagated_as_agent_loop_error() {
        // Mid-stream provider error after accumulated text. Under the
        // Phase-D recovery flow this enters R1 (ContinueText), retries up
        // to the stream-retry budget, and finally surfaces a
        // StreamInterrupted error when every attempt reproduces the fault.
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::TextDelta("before error".into())),
            Err(InferenceExecutionError::Provider("rate limited".into())),
        ]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let err = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
            .await
            .unwrap_err();

        match err {
            AgentLoopError::InferenceFailed(msg) => {
                assert!(
                    msg.contains("stream interrupted"),
                    "expected stream-interrupt message, got: {msg}"
                );
            }
            other => panic!("expected InferenceFailed, got: {other:?}"),
        }
    }

    // -- ToolCallReady event emitted after complete tool args --

    #[tokio::test]
    async fn emits_tool_call_ready_event_for_complete_tool() {
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::ToolCallStart {
                id: "tc1".into(),
                name: "calculator".into(),
            }),
            Ok(LlmStreamEvent::ToolCallDelta {
                id: "tc1".into(),
                args_delta: r#"{"expr":"1+1"}"#.into(),
            }),
            Ok(LlmStreamEvent::Stop(StopReason::ToolUse)),
        ]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
            .await
            .unwrap();

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallReady { id, name, .. } if id == "tc1" && name == "calculator"
        )));
    }

    // ========================================================================
    // Fault injection — executor failure modes
    // ========================================================================

    // -- Error mid-stream after N successful events --

    struct FailAfterNEventsExecutor {
        events_before_fail: usize,
    }

    #[async_trait]
    impl awaken_contract::contract::executor::LlmExecutor for FailAfterNEventsExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Err(InferenceExecutionError::Provider("not implemented".into()))
        }

        fn execute_stream(
            &self,
            _request: InferenceRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<InferenceStream, InferenceExecutionError>>
                    + Send
                    + '_,
            >,
        > {
            let n = self.events_before_fail;
            Box::pin(async move {
                let mut events: Vec<Result<LlmStreamEvent, InferenceExecutionError>> = Vec::new();
                for i in 0..n {
                    events.push(Ok(LlmStreamEvent::TextDelta(format!("chunk-{i}"))));
                }
                events.push(Err(InferenceExecutionError::Provider(
                    "injected mid-stream failure".into(),
                )));
                Ok(Box::pin(futures::stream::iter(events)) as InferenceStream)
            })
        }

        fn name(&self) -> &str {
            "fail-after-n"
        }
    }

    fn make_failing_agent(events_before_fail: usize) -> ResolvedAgent {
        ResolvedAgent::new(
            "test-agent",
            "test-model",
            "system prompt",
            Arc::new(FailAfterNEventsExecutor { events_before_fail }),
        )
    }

    #[tokio::test]
    async fn error_after_zero_events_returns_inference_failed() {
        // 0 successful events + error → R4 (WholeRestart). The recovery
        // loop emits `StreamReset` for each retry then surfaces
        // `StreamInterrupted` once the budget exhausts.
        let agent = make_failing_agent(0);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let err = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
            .await
            .unwrap_err();

        match err {
            AgentLoopError::InferenceFailed(msg) => {
                assert!(
                    msg.contains("stream interrupted"),
                    "expected stream-interrupt message, got: {msg}"
                );
            }
            other => panic!("expected InferenceFailed, got: {other:?}"),
        }

        let events = sink.take();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::StreamReset { .. })),
            "expected at least one StreamReset event, got: {events:?}"
        );
    }

    #[tokio::test]
    async fn error_after_n_events_emits_partial_deltas_then_fails() {
        let agent = make_failing_agent(3);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let err = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
            .await
            .unwrap_err();

        assert!(matches!(err, AgentLoopError::InferenceFailed(_)));

        // At least 3 TextDelta events should have been emitted before the
        // first error. Retries under the R1 recovery plan may emit more
        // duplicated deltas across attempts; we assert the floor rather
        // than an exact count so the test stays agnostic to retry budget.
        let events = sink.take();
        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TextDelta { .. }))
            .collect();
        assert!(
            text_deltas.len() >= 3,
            "expected >=3 text deltas (with possible retries), got {}",
            text_deltas.len()
        );
    }

    // -- Executor that immediately fails at execute_stream level --

    struct ImmediateStreamFailExecutor;

    #[async_trait]
    impl awaken_contract::contract::executor::LlmExecutor for ImmediateStreamFailExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Err(InferenceExecutionError::Provider("execute failed".into()))
        }

        fn execute_stream(
            &self,
            _request: InferenceRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<InferenceStream, InferenceExecutionError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                Err(InferenceExecutionError::Provider(
                    "stream creation failed".into(),
                ))
            })
        }

        fn name(&self) -> &str {
            "immediate-fail"
        }
    }

    #[tokio::test]
    async fn executor_stream_creation_failure_surfaces_as_error() {
        let agent = ResolvedAgent::new(
            "test-agent",
            "test-model",
            "system prompt",
            Arc::new(ImmediateStreamFailExecutor),
        );
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let err = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
            .await
            .unwrap_err();

        match err {
            AgentLoopError::InferenceFailed(msg) => {
                assert!(msg.contains("stream creation failed"));
            }
            other => panic!("expected InferenceFailed, got: {other:?}"),
        }
    }

    // -- Executor returns different error types --

    #[tokio::test]
    async fn rate_limited_error_surfaces_correctly() {
        // Rate-limit mid-stream retries through R4 (WholeRestart) since no
        // deltas are accumulated yet when the error fires. After the
        // stream retry budget is exhausted the caller sees a
        // stream-interrupted error.
        let agent = make_agent(vec![Err(InferenceExecutionError::rate_limited(
            "429 too many requests",
        ))]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let err = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
            .await
            .unwrap_err();

        match err {
            AgentLoopError::InferenceFailed(msg) => {
                assert!(
                    msg.contains("stream interrupted"),
                    "expected stream-interrupt message, got: {msg}"
                );
            }
            other => panic!("expected InferenceFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_error_surfaces_correctly() {
        // Timeout mid-stream routes through the recovery loop and
        // surfaces as `stream interrupted` after the budget exhausts.
        let agent = make_agent(vec![Err(InferenceExecutionError::Timeout(
            "30s exceeded".into(),
        ))]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let err = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
            .await
            .unwrap_err();

        match err {
            AgentLoopError::InferenceFailed(msg) => {
                assert!(
                    msg.contains("stream interrupted"),
                    "expected stream-interrupt message, got: {msg}"
                );
                // original classifier info is preserved in snapshot cause (connection reset for mapped Timeout).
                let _ = "30s exceeded"; // keep literal for test discoverability
            }
            other => panic!("expected InferenceFailed, got: {other:?}"),
        }
    }

    // -- Hanging executor with cancellation token --

    struct HangingExecutor;

    #[async_trait]
    impl awaken_contract::contract::executor::LlmExecutor for HangingExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            std::future::pending::<()>().await;
            unreachable!()
        }

        fn execute_stream(
            &self,
            _request: InferenceRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<InferenceStream, InferenceExecutionError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                // Return a stream that never yields
                let stream = futures::stream::pending();
                Ok(Box::pin(stream) as InferenceStream)
            })
        }

        fn name(&self) -> &str {
            "hanging"
        }
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_executor_is_caught_by_cancellation_token() {
        let agent = ResolvedAgent::new(
            "test-agent",
            "test-model",
            "system prompt",
            Arc::new(HangingExecutor),
        );
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Schedule cancellation after 5 seconds
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            token_clone.cancel();
        });

        let (result, _hint) = execute_streaming(
            &agent,
            make_request(),
            &sink,
            Some(&token),
            &mut it,
            &mut ot,
            None,
        )
        .await
        .unwrap();

        // Cancelled runs return EndTurn, no panic, no hang
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        assert!(result.content.is_empty());
        assert!(result.tool_calls.is_empty());
    }

    // -- Error after tool call start but before args complete --

    #[tokio::test]
    async fn error_mid_tool_call_returns_inference_error() {
        // ToolCallStart + partial ToolCallDelta + mid-stream error →
        // snapshot has an in-flight tool but no completed tools and no
        // text. That's R4 (WholeRestart): emit StreamReset, retry. All
        // retries reproduce the same failure and the stream retry budget
        // exhausts into a stream-interrupt error.
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::ToolCallStart {
                id: "tc1".into(),
                name: "search".into(),
            }),
            Ok(LlmStreamEvent::ToolCallDelta {
                id: "tc1".into(),
                args_delta: r#"{"q":"partial"#.into(),
            }),
            Err(InferenceExecutionError::Provider("connection reset".into())),
        ]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let err = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
            .await
            .unwrap_err();

        match err {
            AgentLoopError::InferenceFailed(msg) => {
                assert!(
                    msg.contains("stream interrupted"),
                    "expected stream-interrupt message, got: {msg}"
                );
            }
            other => panic!("expected InferenceFailed, got: {other:?}"),
        }

        // Events before the error should still have been emitted, and
        // a StreamReset event should appear from the R4 recovery path.
        let events = sink.take();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCallStart { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::StreamReset { .. }))
        );
    }

    // ========================================================================
    // Phase-F failure-injection harness + R1-R4 matrix
    //
    // These tests exercise the stream-level retry loop introduced in Phase D.
    // A per-attempt scripted executor lets us express "first attempt fails
    // like X, second attempt succeeds like Y" without resorting to time or
    // real transport. Each recovery plan (R1/R2/R3/R4), the idle-stall path,
    // and the retry-budget exhaustion path has its own test.
    // ========================================================================

    // --- R1: pure text interruption → continuation retry succeeds --------

    #[tokio::test]
    async fn r1_text_only_interruption_recovers_via_continuation() {
        let exec = Arc::new(ScriptedPerAttemptExecutor::new(vec![
            // Attempt 1: partial text + mid-stream failure
            vec![
                Ok(LlmStreamEvent::TextDelta("Hello, ".into())),
                Ok(LlmStreamEvent::TextDelta("this is".into())),
                Err(InferenceExecutionError::Provider("connection reset".into())),
            ],
            // Attempt 2: fresh completion (model picks up from continuation)
            vec![
                Ok(LlmStreamEvent::TextDelta(" the second half.".into())),
                Ok(LlmStreamEvent::Stop(StopReason::EndTurn)),
            ],
        ]));
        let agent = agent_with(exec.clone());
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) =
            execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
                .await
                .expect("R1 should succeed after one retry");

        assert_eq!(exec.attempts(), 2, "expected exactly two attempts");
        // The second attempt's deltas are preserved in the returned result.
        assert_eq!(result.text(), " the second half.");
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));

        // No StreamReset / ToolCallCancel on the R1 path.
        let events = sink.take();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::StreamReset { .. })),
            "R1 must not emit StreamReset"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCallCancel { .. })),
            "R1 must not emit ToolCallCancel"
        );
    }

    // --- R2: completed tool + partial tool → synthesize tool_use ---------

    #[tokio::test]
    async fn r2_completed_tool_synthesizes_tool_use_without_another_round_trip() {
        let exec = Arc::new(ScriptedPerAttemptExecutor::new(vec![
            // Attempt 1: completed tool A + partial tool B + failure.
            vec![
                Ok(LlmStreamEvent::ToolCallStart {
                    id: "a".into(),
                    name: "search".into(),
                }),
                Ok(LlmStreamEvent::ToolCallDelta {
                    id: "a".into(),
                    args_delta: r#"{"q":"rust"}"#.into(),
                }),
                Ok(LlmStreamEvent::ToolCallStart {
                    id: "b".into(),
                    name: "fetch".into(),
                }),
                Ok(LlmStreamEvent::ToolCallDelta {
                    id: "b".into(),
                    args_delta: r#"{"url":"#.into(), // unclosed
                }),
                Err(InferenceExecutionError::Provider("connection reset".into())),
            ],
            // If R2 is correct we should never see attempt 2: synthesize
            // tool_use short-circuits the retry loop. Put an obvious trap.
            vec![Err(InferenceExecutionError::Provider(
                "R2 should not retry".into(),
            ))],
        ]));
        let agent = agent_with(exec.clone());
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) =
            execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
                .await
                .expect("R2 short-circuits to synthesized tool_use");

        assert_eq!(exec.attempts(), 1, "R2 must not trigger a retry");
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "a");
        assert_eq!(result.tool_calls[0].name, "search");

        let events = sink.take();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCallCancel { id, name, .. }
                    if id == "b" && name == "fetch")),
            "expected ToolCallCancel for the in-flight tool"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCallReady { id, .. } if id == "a")),
            "expected ToolCallReady for the completed tool"
        );
    }

    // --- R3: text + unclosed tool → truncate + continuation --------------

    #[tokio::test]
    async fn r3_text_plus_partial_tool_truncates_and_continues() {
        let exec = Arc::new(ScriptedPerAttemptExecutor::new(vec![
            // Attempt 1: text prefix + unclosed tool + failure
            vec![
                Ok(LlmStreamEvent::TextDelta("Looking it up: ".into())),
                Ok(LlmStreamEvent::ToolCallStart {
                    id: "t1".into(),
                    name: "lookup".into(),
                }),
                Ok(LlmStreamEvent::ToolCallDelta {
                    id: "t1".into(),
                    args_delta: r#"{"id":"#.into(),
                }),
                Err(InferenceExecutionError::Provider("connection reset".into())),
            ],
            // Attempt 2: model continues with a different plan (just text).
            vec![
                Ok(LlmStreamEvent::TextDelta("done.".into())),
                Ok(LlmStreamEvent::Stop(StopReason::EndTurn)),
            ],
        ]));
        let agent = agent_with(exec.clone());
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) =
            execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
                .await
                .expect("R3 recovers after truncation");

        assert_eq!(exec.attempts(), 2);
        assert_eq!(result.text(), "done.");

        let events = sink.take();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCallCancel { id, name, .. }
                    if id == "t1" && name == "lookup")),
            "R3 must emit ToolCallCancel for the unclosed tool"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::StreamReset { .. })),
            "R3 must NOT emit StreamReset"
        );
    }

    // --- R4: nothing salvageable → whole restart + StreamReset -----------

    #[tokio::test]
    async fn r4_empty_snapshot_whole_restarts_and_emits_stream_reset() {
        let exec = Arc::new(ScriptedPerAttemptExecutor::new(vec![
            // Attempt 1: immediate failure, no accumulated state
            vec![Err(InferenceExecutionError::Provider("reset".into()))],
            // Attempt 2: succeeds cleanly
            vec![
                Ok(LlmStreamEvent::TextDelta("fresh start".into())),
                Ok(LlmStreamEvent::Stop(StopReason::EndTurn)),
            ],
        ]));
        let agent = agent_with(exec.clone());
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) =
            execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
                .await
                .expect("R4 recovers after whole restart");

        assert_eq!(exec.attempts(), 2);
        assert_eq!(result.text(), "fresh start");

        let events = sink.take();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::StreamReset { .. })),
            "R4 must emit StreamReset"
        );
    }

    // --- Budget exhaustion → StreamInterrupted ---------------------------

    #[tokio::test]
    async fn retry_budget_exhausted_surfaces_stream_interrupted() {
        // Every attempt fails. Default max_stream_retries = 2, so we expect
        // 3 total attempts (1 initial + 2 retries) before the error
        // surfaces.
        let exec = Arc::new(ScriptedPerAttemptExecutor::new(vec![vec![Err(
            InferenceExecutionError::Provider("reset".into()),
        )]]));
        let agent = agent_with(exec.clone());
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let err = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None)
            .await
            .unwrap_err();

        assert_eq!(
            exec.attempts(),
            3,
            "expected 1 initial + 2 retries = 3 attempts"
        );
        match err {
            AgentLoopError::InferenceFailed(msg) => {
                assert!(
                    msg.contains("stream interrupted"),
                    "expected stream-interrupt message, got: {msg}"
                );
            }
            other => panic!("expected InferenceFailed, got: {other:?}"),
        }
    }

    // --- Idle-stall: hung stream triggers IdleStall cause ---------------

    /// Executor that returns a stream which yields one event and then
    /// never yields again, exercising the idle-stall timeout branch in
    /// `drive_one_stream`. We use `tokio::time::advance` under
    /// `tokio::test(start_paused = true)` to avoid wall-clock waits.
    struct StallingExecutor {
        attempt: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl awaken_contract::contract::executor::LlmExecutor for StallingExecutor {
        async fn execute(
            &self,
            _r: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Err(InferenceExecutionError::Provider("unused".into()))
        }

        fn execute_stream(
            &self,
            _request: InferenceRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<InferenceStream, InferenceExecutionError>>
                    + Send
                    + '_,
            >,
        > {
            let n = self
                .attempt
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    // Attempt 1: one text delta then hang forever.
                    let hung = futures::stream::unfold((), |()| async move {
                        // Never yields — the select! / timeout in
                        // drive_one_stream is responsible for noticing.
                        futures::future::pending::<()>().await;
                        None
                    });
                    let prefix: Vec<Result<LlmStreamEvent, InferenceExecutionError>> =
                        vec![Ok(LlmStreamEvent::TextDelta("partial".into()))];
                    let combined = futures::stream::iter(prefix)
                        .chain(hung)
                        .map(|r: Result<LlmStreamEvent, InferenceExecutionError>| r);
                    Ok(Box::pin(combined) as InferenceStream)
                } else {
                    // Attempt 2: clean finish.
                    let events: Vec<Result<LlmStreamEvent, InferenceExecutionError>> = vec![
                        Ok(LlmStreamEvent::TextDelta(" done.".into())),
                        Ok(LlmStreamEvent::Stop(StopReason::EndTurn)),
                    ];
                    Ok(Box::pin(futures::stream::iter(events)) as InferenceStream)
                }
            })
        }

        fn name(&self) -> &str {
            "stalling"
        }
    }

    #[tokio::test(start_paused = true)]
    async fn idle_stall_triggers_recovery_and_second_attempt_succeeds() {
        let exec = Arc::new(StallingExecutor {
            attempt: std::sync::atomic::AtomicUsize::new(0),
        });
        let agent = ResolvedAgent::new("test-agent", "test-model", "system prompt", exec.clone());
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        // Drive the streaming call concurrently so we can advance paused
        // time past the idle-stall threshold (60s by default).
        let exec_fut =
            execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot, None);
        let drive = async {
            // Wait for the first TextDelta to be emitted, then advance
            // past the idle threshold to trigger the stall.
            tokio::time::sleep(Duration::from_millis(1)).await;
            tokio::time::advance(Duration::from_secs(70)).await;
        };

        let (outcome, ()) = tokio::join!(exec_fut, drive);
        let (result, _hint) = outcome.expect("idle-stall should recover");
        assert_eq!(
            exec.attempt.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "expected 2 attempts after stall recovery"
        );
        assert!(result.text().contains("done"));
    }

    #[test]
    fn idle_timeout_for_doubles_on_thinking_model_names() {
        let policy = LlmRetryPolicy::default().with_stream_idle_timeout_secs(30);
        let base = Duration::from_secs(30);
        let plain = InferenceRequest {
            upstream_model: "gpt-4o-mini".into(),
            routing_key: None,
            messages: vec![],
            tools: vec![],
            system: vec![],
            overrides: None,
            enable_prompt_cache: false,
        };
        assert_eq!(idle_timeout_for(&plain, &policy), base);

        let thinking = InferenceRequest {
            upstream_model: "claude-opus-4-7-thinking".into(),
            ..plain.clone()
        };
        assert_eq!(idle_timeout_for(&thinking, &policy), base * 2);

        let reasoning = InferenceRequest {
            upstream_model: "o1-mini".into(),
            ..plain.clone()
        };
        assert_eq!(idle_timeout_for(&reasoning, &policy), base * 2);

        let o3 = InferenceRequest {
            upstream_model: "o3-preview".into(),
            ..plain.clone()
        };
        assert_eq!(idle_timeout_for(&o3, &policy), base * 2);
    }

    // -----------------------------------------------------------------------
    // GOAWAY / transport-message classification
    // -----------------------------------------------------------------------

    #[test]
    fn classify_mid_stream_maps_goaway_substring_to_goaway_cause() {
        let err = InferenceExecutionError::Provider("HTTP/2 GOAWAY frame received".into());
        assert!(matches!(
            classify_mid_stream(&err),
            Some(InterruptCause::GoAway)
        ));
    }

    #[test]
    fn classify_mid_stream_maps_connection_reset_substring_to_connection_reset() {
        let err = InferenceExecutionError::Provider("ECONNRESET: connection reset by peer".into());
        assert!(matches!(
            classify_mid_stream(&err),
            Some(InterruptCause::ConnectionReset)
        ));
    }

    #[test]
    fn classify_mid_stream_maps_503_substring_to_provider_5xx() {
        let err = InferenceExecutionError::Provider("503 Service Unavailable".into());
        assert!(matches!(
            classify_mid_stream(&err),
            Some(InterruptCause::Provider5xxMidStream(_))
        ));
    }

    #[test]
    fn classify_mid_stream_preserves_cause_from_stream_interrupted() {
        let err = InferenceExecutionError::StreamInterrupted {
            cause: InterruptCause::IdleStall,
            snapshot: Box::new(InterruptSnapshot {
                text: None,
                completed_tool_calls: vec![],
                in_flight_tool: None,
                bytes_received: 0,
            }),
        };
        assert!(matches!(
            classify_mid_stream(&err),
            Some(InterruptCause::IdleStall)
        ));
    }

    #[test]
    fn classify_mid_stream_refuses_permanent_errors() {
        assert!(
            classify_mid_stream(&InferenceExecutionError::ContextOverflow("x".into())).is_none()
        );
        assert!(classify_mid_stream(&InferenceExecutionError::Unauthorized("x".into())).is_none());
        assert!(
            classify_mid_stream(&InferenceExecutionError::ContentFiltered("x".into())).is_none()
        );
        assert!(classify_mid_stream(&InferenceExecutionError::Cancelled).is_none());
    }

    // -----------------------------------------------------------------------
    // J: Cancellation during retry backoff aborts the retry loop
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // L: Cross-process stream resume via `StreamCheckpointStore`
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn checkpoint_is_flushed_on_mid_stream_interruption() {
        use awaken_contract::contract::stream_checkpoint::{
            InMemoryStreamCheckpointStore, StreamCheckpointStore,
        };

        // Attempt 1 emits 8 text deltas then fails. With
        // CHECKPOINT_FLUSH_DELTAS = 4 the writer must flush at least
        // twice (after delta #4 and after delta #8). On mid-stream
        // error we also flush once more before surfacing.
        let deltas: Vec<Result<LlmStreamEvent, InferenceExecutionError>> = (0..8)
            .map(|i| Ok(LlmStreamEvent::TextDelta(format!("d{i}"))))
            .chain(std::iter::once(Err(InferenceExecutionError::Provider(
                "reset".into(),
            ))))
            .collect();
        let exec = Arc::new(ScriptedPerAttemptExecutor::new(vec![
            deltas.clone(),
            deltas,
        ]));
        let agent = agent_with(exec.clone());
        let sink = VecEventSink::new();
        let store: Arc<InMemoryStreamCheckpointStore> =
            Arc::new(InMemoryStreamCheckpointStore::new());
        let handle = CheckpointHandle {
            store: store.as_ref(),
            run_id: "run-checkpoint-flush",
            thread_id: "thread-1",
        };
        let mut it = 0u64;
        let mut ot = 0u64;

        // Budget 0: exhaust on first attempt so the failure surfaces
        // and we can assert on the persisted checkpoint.
        let _ = execute_streaming(
            &agent,
            make_request(),
            &sink,
            None,
            &mut it,
            &mut ot,
            Some(handle),
        )
        .await;

        let saved = store
            .get("run-checkpoint-flush")
            .await
            .unwrap()
            .expect("checkpoint must have been persisted before failure");
        assert_eq!(saved.run_id, "run-checkpoint-flush");
        assert_eq!(saved.thread_id, "thread-1");
        assert!(
            saved.partial_text.contains("d0") && saved.partial_text.contains("d7"),
            "partial_text should contain all 8 deltas, got: {}",
            saved.partial_text
        );
    }

    #[tokio::test]
    async fn cross_process_resume_injects_continuation_from_checkpoint() {
        use awaken_contract::contract::stream_checkpoint::{
            InMemoryStreamCheckpointStore, StreamCheckpoint, StreamCheckpointStore,
        };

        // Pre-populate a checkpoint as though a prior process crashed
        // mid-stream. The executor records every InferenceRequest it
        // receives so we can assert the continuation prompt was
        // injected.
        let store: Arc<InMemoryStreamCheckpointStore> =
            Arc::new(InMemoryStreamCheckpointStore::new());
        store
            .put(StreamCheckpoint {
                run_id: "run-resumed".into(),
                thread_id: "thread-1".into(),
                upstream_model: "test-model".into(),
                partial_text: "half-written answer".into(),
                completed_tool_calls: vec![],
                in_flight_tool: None,
                updated_at_ms: 1_000,
            })
            .await
            .unwrap();

        // Capturing executor: records each request, returns a clean
        // terminal stream so the resumed call completes immediately.
        struct CapturingExec {
            captured: Arc<std::sync::Mutex<Vec<InferenceRequest>>>,
        }

        #[async_trait]
        impl awaken_contract::contract::executor::LlmExecutor for CapturingExec {
            async fn execute(
                &self,
                _r: InferenceRequest,
            ) -> Result<StreamResult, InferenceExecutionError> {
                Err(InferenceExecutionError::Provider("unused".into()))
            }

            fn execute_stream(
                &self,
                request: InferenceRequest,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<InferenceStream, InferenceExecutionError>,
                        > + Send
                        + '_,
                >,
            > {
                self.captured.lock().unwrap().push(request);
                Box::pin(async move {
                    let events: Vec<Result<LlmStreamEvent, InferenceExecutionError>> = vec![
                        Ok(LlmStreamEvent::TextDelta(" — conclusion.".into())),
                        Ok(LlmStreamEvent::Stop(StopReason::EndTurn)),
                    ];
                    Ok(Box::pin(futures::stream::iter(events)) as InferenceStream)
                })
            }

            fn name(&self) -> &str {
                "capturing"
            }
        }

        let captured: Arc<std::sync::Mutex<Vec<InferenceRequest>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let exec = Arc::new(CapturingExec {
            captured: captured.clone(),
        });
        let agent = ResolvedAgent::new("test", "test-model", "sys", exec);
        let sink = VecEventSink::new();
        let handle = CheckpointHandle {
            store: store.as_ref(),
            run_id: "run-resumed",
            thread_id: "thread-1",
        };
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) = execute_streaming(
            &agent,
            make_request(),
            &sink,
            None,
            &mut it,
            &mut ot,
            Some(handle),
        )
        .await
        .expect("resume should succeed");

        // The executor was called exactly once, with a request whose
        // messages end in `assistant("half-written answer")` +
        // `user(<continuation prompt>)` — the R1 restore pattern.
        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            let last_two: Vec<_> = reqs[0]
                .messages
                .iter()
                .rev()
                .take(2)
                .rev()
                .cloned()
                .collect();
            assert_eq!(last_two.len(), 2);
            assert_eq!(
                last_two[0].text(),
                "half-written answer",
                "assistant prefix must carry saved partial text"
            );
            assert!(
                last_two[1].text().contains("interrupted mid-stream"),
                "user continuation prompt must follow the prefix, got: {}",
                last_two[1].text()
            );
        } // drop MutexGuard before the await below

        // The fresh attempt's output wins: the text is whatever the
        // resumed attempt produced.
        assert_eq!(result.text(), " — conclusion.");
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));

        // Checkpoint must be cleared on clean completion — otherwise
        // subsequent runs would incorrectly restore it.
        assert!(
            store.get("run-resumed").await.unwrap().is_none(),
            "checkpoint must be deleted after successful resume"
        );
    }

    #[tokio::test]
    async fn cross_process_resume_with_completed_tool_checkpoint_short_circuits_to_tool_use() {
        use awaken_contract::contract::stream_checkpoint::{
            InMemoryStreamCheckpointStore, StreamCheckpoint, StreamCheckpointStore,
        };
        use serde_json::json;

        let store: Arc<InMemoryStreamCheckpointStore> =
            Arc::new(InMemoryStreamCheckpointStore::new());
        store
            .put(StreamCheckpoint {
                run_id: "run-r2-resumed".into(),
                thread_id: "thread-1".into(),
                upstream_model: "test-model".into(),
                partial_text: "thinking...".into(),
                completed_tool_calls: vec![ToolCall::new("tc-1", "search", json!({"q": "rust"}))],
                in_flight_tool: None,
                updated_at_ms: 1_000,
            })
            .await
            .unwrap();

        // An executor that PANICS if called — the R2 short-circuit
        // must not reopen a stream.
        struct NeverCallMe;

        #[async_trait]
        impl awaken_contract::contract::executor::LlmExecutor for NeverCallMe {
            async fn execute(
                &self,
                _r: InferenceRequest,
            ) -> Result<StreamResult, InferenceExecutionError> {
                panic!("R2 checkpoint resume must not reopen a stream");
            }

            fn execute_stream(
                &self,
                _r: InferenceRequest,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<InferenceStream, InferenceExecutionError>,
                        > + Send
                        + '_,
                >,
            > {
                panic!("R2 checkpoint resume must not reopen a stream");
            }

            fn name(&self) -> &str {
                "never-call"
            }
        }

        let agent = ResolvedAgent::new("test", "test-model", "sys", Arc::new(NeverCallMe));
        let sink = VecEventSink::new();
        let handle = CheckpointHandle {
            store: store.as_ref(),
            run_id: "run-r2-resumed",
            thread_id: "thread-1",
        };
        let mut it = 0u64;
        let mut ot = 0u64;

        let (result, _hint) = execute_streaming(
            &agent,
            make_request(),
            &sink,
            None,
            &mut it,
            &mut ot,
            Some(handle),
        )
        .await
        .expect("R2 resume should short-circuit successfully");

        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "search");
        assert_eq!(result.text(), "thinking...");

        // Checkpoint cleared on R2 resume (consumed).
        assert!(store.get("run-r2-resumed").await.unwrap().is_none());

        // Sink should have observed the ToolCallReady event for the
        // resumed tool so downstream consumers see the same sequence
        // as a normal `StopReason::ToolUse` termination.
        let events = sink.events();
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::ToolCallReady { id, .. } if id == "tc-1"
            )),
            "expected ToolCallReady for the resumed tool"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_during_backoff_aborts_retry_loop_with_cancelled_error() {
        use crate::cancellation::CancellationToken;

        // R4-path executor: first attempt fails immediately with no
        // accumulated state. With default policy the retry loop sleeps
        // before attempt 2; the cancellation token fires during that
        // sleep and the error surfaces as `Cancelled`, not as
        // `StreamInterrupted`.
        let exec = Arc::new(ScriptedPerAttemptExecutor::new(vec![
            vec![Err(InferenceExecutionError::Provider("reset".into()))],
            vec![Err(InferenceExecutionError::Provider(
                "should-not-be-reached".into(),
            ))],
        ]));
        let agent = agent_with(exec.clone());
        let sink = VecEventSink::new();
        let token = CancellationToken::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        // Kick off the streaming call and cancel mid-backoff.
        let exec_fut = execute_streaming(
            &agent,
            make_request(),
            &sink,
            Some(&token),
            &mut it,
            &mut ot,
            None,
        );
        let drive = async {
            // Let the first attempt open and fail.
            tokio::time::sleep(Duration::from_millis(1)).await;
            // Cancel before the backoff sleep completes. The default
            // stream retry backoff for ConnectionReset ends up using
            // the general `delay_before_retry` path, so sleeping any
            // paused duration >= 1s guarantees we're inside it.
            token.cancel();
            tokio::time::advance(Duration::from_secs(30)).await;
        };

        let (result, ()) = tokio::join!(exec_fut, drive);
        let err = result.expect_err("cancellation must abort the retry loop");
        match err {
            AgentLoopError::InferenceFailed(msg) => {
                assert!(
                    msg.contains("cancelled"),
                    "expected cancellation message, got: {msg}"
                );
            }
            other => panic!("expected InferenceFailed(cancelled), got: {other:?}"),
        }
        // Only the first attempt should have run.
        assert_eq!(exec.attempts(), 1, "retry must not proceed after cancel");
    }
}
