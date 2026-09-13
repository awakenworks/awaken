//! The model-inference call seam: retry + circuit-breaker + streaming +
//! mid-stream recovery (R1–R4) + durable checkpoint flush, plus the OTel GenAI
//! `chat` span. Split out of `engine/mod.rs` (the step-loop driver) so the
//! "call the provider" responsibility is isolated from "drive the loop" and the
//! module stays within the file-length limit. Items are shared through
//! `use super::*` (same-crate privates included).

use super::*;
use awaken_runtime_contract::resilience::{
    Classify, RETRY_ACTIVE_STATE, RetryDecision, bounded_retry_decision,
};

/// Sole admission gate for retiring a spent interrupted-stream checkpoint.
/// The async store effect remains with the caller; this kernel only decides
/// whether the completed local observation is safe to acknowledge.
pub(super) const fn should_clear_spent_checkpoint(
    cancelled: bool,
    journal_failed: bool,
    can_acknowledge_return: bool,
) -> bool {
    !cancelled && !journal_failed && can_acknowledge_return
}

/// Cancellation may discard a read/provider future, never an in-flight durable
/// evidence write. This is the model seam's one cancellable-await boundary.
pub(super) async fn model_read<T>(
    token: Option<&tokio_util::sync::CancellationToken>,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    match token {
        Some(token) => tokio::select! {
            biased;
            _ = token.cancelled() => None,
            result = future => Some(result),
        },
        None => Some(future.await),
    }
}

/// Rebuild a request that carries the confirmed partial as an assistant prefix
/// followed by a continuation prompt, so the model continues rather than
/// regenerates. Request-only: these two messages are never committed to the
/// transcript — a transient interruption is not a real step boundary.
fn continuation_request(request: &ChatRequest, prefix: &str) -> ChatRequest {
    let mut messages = request.messages.clone();
    messages.push(ChatMessage {
        role: Role::Assistant,
        content: vec![ContentBlock::text(prefix.to_string())],
    });
    messages.push(ChatMessage {
        role: Role::User,
        content: vec![ContentBlock::text(STREAM_CONTINUATION_PROMPT.to_string())],
    });
    ChatRequest {
        messages,
        ..request.clone()
    }
}

/// Prepend the confirmed partial `prefix` onto a continued response so the
/// committed step is the whole text. An empty prefix returns the response
/// unchanged — the common, non-interrupted path pays nothing.
fn stitch_prefix(response: ChatResponse, prefix: &str) -> ChatResponse {
    if prefix.is_empty() {
        return response;
    }
    let mut blocks = response.output.blocks;
    match blocks.first_mut() {
        Some(ContentBlock::Text { text }) => *text = format!("{prefix}{text}"),
        _ => blocks.insert(0, ContentBlock::text(prefix.to_string())),
    }
    ChatResponse {
        output: AssistantOutput::from_blocks(blocks),
        ..response
    }
}

/// Call inference, streaming chunks to `sink` as they arrive and retrying a
/// retryable failure per the policy (exponential backoff with jitter, honoring a
/// server `Retry-After`). A permanent failure returns immediately (G26). Every
/// attempt first passes the model's circuit breaker: while its circuit is open
/// the call fails fast as a retryable provider error instead of burning the
/// retry budget against a provider that is already down.
///
/// Only after positive termination evidence, an interrupted stream is recovered from its partial rather than
/// regenerated: text-only continues from an assistant prefix + continuation
/// prompt (R1); a still-open tool call is dropped and the text continues (R3);
/// tool calls that finished before the drop are executed without re-inferring
/// (R2); nothing salvageable restarts clean (R4). When a `checkpoint` store is
/// wired, the partial is flushed durably at the interruption boundary (Phase 3)
/// so a crash *during* recovery resumes in a fresh process (via `resume`) instead
/// of re-running the step. The checkpoint is deleted the instant recovery
/// concludes here — it survives only the crash window this guards.
// The `chat` inference span lives at this retry seam, not inside a provider, so
// every executor (echo/probe as well as the real genai client) yields one span per
// logical model call. It follows the OTel GenAI semantic conventions: span name
// `chat {model}`, `SpanKind::Client`, `gen_ai.operation.name = "chat"`, and the
// `gen_ai.usage.*` / `gen_ai.response.finish_reasons` recorded from the committed
// `ChatResponse`. Retries happen inside `infer_with_retry_inner`, within this span.
// `gen_ai.provider.name` is the neutral `awaken` (the runtime never names a concrete
// provider SDK — G22); the bound model is the real routing identity.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(super) async fn infer_with_retry(
    llm: &std::sync::Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
    request: ChatRequest,
    policy: &crate::retry::LlmRetryPolicy,
    breaker: &crate::circuit_breaker::CircuitBreaker,
    sink: &dyn DeltaSink,
    checkpoint: Option<&CheckpointCtx<'_>>,
    resume: Option<StreamCheckpoint>,
    capture: &awaken_runtime_contract::CaptureDecision,
    content_sink: content::SinkTarget<'_>,
    metrics: &dyn awaken_runtime_contract::metrics::MetricsRecorder,
) -> std::result::Result<ChatResponse, awaken_runtime_contract::llm::Error> {
    infer_with_retry_observed(
        llm,
        request,
        policy,
        breaker,
        sink,
        checkpoint,
        resume,
        capture,
        content_sink,
        metrics,
        None,
        None,
    )
    .await
    .expect("test checkpoint retirement succeeds")
    .result
}

pub(super) struct ObservedModelRequest {
    pub(super) result: std::result::Result<ChatResponse, awaken_runtime_contract::llm::Error>,
    pub(super) observation: awaken_runtime_contract::llm::ModelRequestObservation,
}

#[tracing::instrument(
    name = "chat",
    skip_all,
    fields(
        otel.name = tracing::field::Empty,
        otel.kind = "client",
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = "awaken",
        gen_ai.request.model = %request.model_binding.model_ref,
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.input.messages = tracing::field::Empty,
        gen_ai.output.messages = tracing::field::Empty,
        error.type = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    )
)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn infer_with_retry_observed(
    llm: &std::sync::Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
    request: ChatRequest,
    policy: &crate::retry::LlmRetryPolicy,
    breaker: &crate::circuit_breaker::CircuitBreaker,
    sink: &dyn DeltaSink,
    checkpoint: Option<&CheckpointCtx<'_>>,
    resume: Option<StreamCheckpoint>,
    capture: &awaken_runtime_contract::CaptureDecision,
    content_sink: content::SinkTarget<'_>,
    metrics: &dyn awaken_runtime_contract::metrics::MetricsRecorder,
    ownership: Option<&dyn awaken_runtime_contract::AttemptOwnershipVerifier>,
    mut journal: Option<&mut super::model_operation::ModelOperationJournal<'_>>,
) -> std::result::Result<
    ObservedModelRequest,
    awaken_agent_contract::stream::checkpoint::StreamCheckpointError,
> {
    let cancellation = journal
        .as_deref()
        .and_then(|journal| journal.context.cancellation.clone());
    let span = tracing::Span::current();
    // The routing model id, captured before `request` moves into the retry loop —
    // it labels both the span and the metric (structure only, never content).
    let model_id = request.model_binding.model_ref.clone();
    let started = std::time::Instant::now();
    // OTel GenAI span name is the templated `{operation} {model}` (SHOULD).
    span.record(
        "otel.name",
        format!("chat {}", request.model_binding.model_ref).as_str(),
    );
    // Content telemetry (ADR-0050): gated + scrubbed; before the request moves.
    model_read(
        cancellation.as_ref(),
        content::emit_content(
            capture,
            content_sink,
            awaken_runtime_contract::ContentKind::InputMessages,
            "gen_ai.input.messages",
            &content::render_chat_messages(&request.messages),
        ),
    )
    .await;
    let mut retry_count = 0;
    let quiescence = journal
        .as_deref()
        .map(|journal| journal.context.physical_quiescence.clone())
        .unwrap_or_default();
    let result = infer_with_retry_inner(
        llm,
        request,
        policy,
        breaker,
        sink,
        checkpoint,
        resume,
        metrics,
        &mut retry_count,
        ownership,
        journal.as_deref_mut(),
        cancellation.as_ref(),
        &quiescence,
    )
    .await;
    // A cancelled observation or failed evidence write has not concluded
    // recovery. Preserve its prior checkpoint instead of deleting the only
    // retained partial merely because the enclosing Rust future returned.
    if should_clear_spent_checkpoint(
        cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled()),
        journal.as_deref().is_some_and(|journal| journal.failed()),
        quiescence.can_acknowledge_return(),
    ) && let Some(ctx) = checkpoint
    {
        // Terminal inference truth must not outrun retirement of its resumable
        // partial. The outer loop maps this storage failure to its existing
        // evidence-unavailable/commit path, which bypasses inference tolerance
        // and terminal `finish`; the unchanged checkpoint remains recoverable.
        ctx.store.delete(&ctx.run_id).await?;
    }
    match &result {
        Ok(response) => {
            if let Some(usage) = &response.usage {
                span.record("gen_ai.usage.input_tokens", usage.prompt_tokens as i64);
                span.record("gen_ai.usage.output_tokens", usage.completion_tokens as i64);
            }
            if let Some(reason) = &response.stop_reason {
                span.record(
                    "gen_ai.response.finish_reasons",
                    format!("{reason:?}").as_str(),
                );
            }
            // Content telemetry (ADR-0050): completion, gated + redactor-scrubbed.
            model_read(
                cancellation.as_ref(),
                content::emit_content(
                    capture,
                    content_sink,
                    awaken_runtime_contract::ContentKind::OutputMessages,
                    "gen_ai.output.messages",
                    &response.output.text_content(),
                ),
            )
            .await;
        }
        // OTel: on a failed inference, tag `error.type` (the neutral taxonomy code)
        // and mark the span status ERROR so the trace surfaces the failure.
        Err(err) => {
            span.record("error.type", err.code());
            span.record("otel.status_code", "ERROR");
            // Structure only: retain enough evidence to distinguish a local
            // deadline from an authority loss without logging provider bodies,
            // prompts, credentials, URLs, or arbitrary error text.
            tracing::warn!(
                error_code = err.code(),
                retryable = err.is_retryable(),
                execution_evidence = ?err.execution_evidence(),
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "model inference returned an error"
            );
        }
    }
    // Structure-only metric at the same chokepoint as the span (#2): model id,
    // outcome class, latency, and token counts — no content, no PII labels.
    let (outcome, input_tokens, output_tokens) = match &result {
        Ok(response) => (
            "ok",
            response.usage.as_ref().map(|u| u.prompt_tokens),
            response.usage.as_ref().map(|u| u.completion_tokens),
        ),
        Err(err) => (err.code(), None, None),
    };
    metrics.record_inference(awaken_runtime_contract::metrics::InferenceMetric {
        model: &model_id,
        outcome,
        duration: started.elapsed(),
        input_tokens,
        output_tokens,
    });
    let observation = awaken_runtime_contract::llm::ModelRequestObservation {
        is_error: result.is_err(),
        usage: result
            .as_ref()
            .ok()
            .and_then(|response| response.usage)
            .unwrap_or_default(),
        retry_count,
    };
    Ok(ObservedModelRequest {
        result,
        observation,
    })
}

#[allow(clippy::too_many_arguments)]
async fn infer_with_retry_inner(
    llm: &std::sync::Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
    request: ChatRequest,
    policy: &crate::retry::LlmRetryPolicy,
    breaker: &crate::circuit_breaker::CircuitBreaker,
    sink: &dyn DeltaSink,
    checkpoint: Option<&CheckpointCtx<'_>>,
    resume: Option<StreamCheckpoint>,
    metrics: &dyn awaken_runtime_contract::metrics::MetricsRecorder,
    retry_count: &mut u32,
    ownership: Option<&dyn awaken_runtime_contract::AttemptOwnershipVerifier>,
    mut journal: Option<&mut super::model_operation::ModelOperationJournal<'_>>,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
    quiescence: &awaken_runtime_contract::runtime_context::AttemptQuiescence,
) -> std::result::Result<ChatResponse, awaken_runtime_contract::llm::Error> {
    // Recovery must check the committed invocation before even salvaging a
    // checkpoint's tool arguments. A fresh process has no in-memory latch.
    if let Some(journal) = journal.as_deref_mut() {
        journal.verify_reentry(resume.is_some())?;
    }
    let model = request.model_binding.model_ref.clone();
    let sink = ContinuationSink::new(sink);
    // Text confirmed from prior interrupted attempts; grows as continuation
    // proceeds. Empty means "start (or restart) clean".
    let mut prefix = String::new();

    // Cross-process resume: reproduce in-process the plan a fresh interruption
    // would have taken from this persisted partial.
    if let Some(cp) = resume {
        *retry_count = cp.retry_count;
        let completed = parse_completed(&cp.partial_tools);
        if !completed.is_empty() {
            // R2: the model had finished its tool calls before the crash — run
            // them without re-inferring.
            return Ok(synthesized_tool_response(&cp.partial_text, completed));
        }
        // R1/R3: continue from the recovered text (any in-flight tool dropped).
        // R4 (empty text) leaves the prefix empty, i.e. a clean start.
        prefix = cp.partial_text;
    }

    let mut attempt = 0;
    loop {
        if cancellation.is_some_and(|token| token.is_cancelled()) {
            return Err(awaken_runtime_contract::llm::Error::Unauthorized(
                "model observation cancelled".into(),
            ));
        }
        let permit = breaker
            .check(&model, metrics)
            .map_err(awaken_runtime_contract::llm::Error::Provider)?;
        let attempt_request = if prefix.is_empty() {
            request.clone()
        } else {
            continuation_request(&request, &prefix)
        };
        sink.reset();
        // The dispatch claim is a live side-effect fence, distinct from the
        // once-per-logical-request application gate. Recheck it immediately
        // before every provider attempt so a lease lost during backoff cannot
        // spend another remote request before the eventual commit CAS notices.
        awaken_runtime_contract::execution::verify_attempt_ownership(ownership)
            .await
            .map_err(|error| {
                awaken_runtime_contract::llm::Error::Unauthorized(format!(
                    "model request attempt no longer owns execution: {error}"
                ))
            })?;
        let operation = if let Some(journal) = journal.as_deref_mut() {
            let identity = journal.enter(&attempt_request.model_binding).await?;
            // Persistence may have waited past a lease boundary. The original
            // ownership port remains the only permission to enter the executor.
            awaken_runtime_contract::execution::verify_attempt_ownership(ownership)
                .await
                .map_err(|error| {
                    awaken_runtime_contract::llm::Error::Unauthorized(error.to_string())
                })?;
            identity
        } else {
            None
        };
        // Journal persistence and ownership verification both await. A sibling
        // may have vetoed this attempt during either wait; its old model receipt
        // is not proof that every participant stopped. Recheck at actual entry.
        if !quiescence.can_acknowledge_return() {
            return Err(awaken_runtime_contract::llm::Error::Provider(
                "attempt participant termination remains unconfirmed".into(),
            ));
        }
        let outcome = model_read(
            cancellation,
            tokio::time::timeout(
                policy.attempt_timeout,
                awaken_runtime_contract::model_operation::with_model_operation_context(
                    operation,
                    quiescence.observe_return(llm.infer_streaming(attempt_request, &sink)),
                ),
            ),
        )
        .await;
        let Some(outcome) = outcome else {
            // A dropped provider future leaves Entered evidence. Do not forge
            // Returned merely because the caller stopped observing.
            return Err(awaken_runtime_contract::llm::Error::Unauthorized(
                "model observation cancelled".into(),
            ));
        };
        use awaken_runtime_contract::model_operation::ModelExecutionEvidence;
        let execution = match &outcome {
            // A returned response IS the adapter's receipt for this
            // invocation; Runtime reads it rather than inferring termination
            // from the fact that a Rust future resolved.
            Ok(Ok(response)) => ModelExecutionEvidence::Terminated(response.terminal_receipt()),
            // A returned error carries only what the adapter could prove.
            Ok(Err(error)) => error.execution_evidence(),
            // The local attempt timeout fired. That is an observation about
            // this process, never about the remote operation.
            Err(_) => ModelExecutionEvidence::Unknown,
        };
        // The adapter can drop its own transport future and RETURN an error.
        // The outer return observer cannot detect that abandonment. Latch the
        // explicit evidence before any awaited commit, retry or checkpoint work.
        if !execution.permits_reentry() {
            quiescence.mark_uncertain();
        }
        if let Some(journal) = journal.as_deref_mut() {
            journal.returned(outcome.is_err(), execution).await?;
        }
        if cancellation.is_some_and(|token| token.is_cancelled()) {
            // Cancellation may have arrived while the return write was held.
            // Do not consume/update a stream checkpoint for a retry that will
            // never be entered.
            return Err(awaken_runtime_contract::llm::Error::Unauthorized(
                "model observation cancelled".into(),
            ));
        }
        let attempt_result = outcome.unwrap_or_else(|_| {
            Err(awaken_runtime_contract::llm::Error::Timeout(format!(
                "inference attempt exceeded {}ms",
                policy.attempt_timeout.as_millis()
            )))
        });
        if !quiescence.can_acknowledge_return() {
            // The watchdog abandoned an in-flight executor. Neither a retry,
            // continuation, nor synthesized tool call may cross this boundary.
            if let Err(error) = &attempt_result {
                permit.failure(error.is_retryable());
            }
            return attempt_result;
        }
        match attempt_result {
            Ok(response) => {
                permit.success();
                return Ok(stitch_prefix(response, &prefix));
            }
            Err(err) => {
                // Only retryable errors speak to provider health; the counted
                // set must stay exactly `is_retryable`, or permanent faults
                // (bad key, overlong prompt) would trip the breaker.
                let retryable = err.is_retryable();
                permit.failure(retryable);
                // Classify the same failure through the unified Disposition (E3-1)
                // for failure-span observability. This never alters the retry/breaker
                // gate above (that stays exactly `is_retryable`); the tuple forces the
                // classification to run regardless of the trace level.
                let disposition = err.disposition();
                let classified = (
                    disposition.is_retryable(),
                    disposition.prefers_failover(),
                    disposition.retry_after(),
                );
                tracing::debug!(?disposition, ?classified, "inference attempt failed");
                let snapshot = sink.snapshot();
                // A transport interruption may leave a confirmed partial. A
                // terminal but invalid response does not: retaining its text or
                // tool fragments would poison every retry with the protocol
                // violation we are trying to recover from. Preserve only a
                // prefix confirmed by an earlier attempt in that case.
                let retain_attempt_output = err.retry_preserves_attempt_output();
                let combined_text = if retain_attempt_output {
                    format!("{prefix}{}", snapshot.text)
                } else {
                    prefix.clone()
                };
                let partial_tools = if retain_attempt_output {
                    snapshot.tools
                } else {
                    Vec::new()
                };

                if retryable {
                    let completed = parse_completed(&partial_tools);
                    // Boundary flush (Phase 3): persist the whole in-flight partial
                    // so a crash during recovery resumes here. The runtime still
                    // degrades deliberately, but the backend error is observable.
                    if let Some(ctx) = checkpoint
                        && let Err(error) = ctx
                            .store
                            .put(ctx.checkpoint(
                                combined_text.clone(),
                                partial_tools.clone(),
                                if completed.is_empty() {
                                    retry_count.saturating_add(1)
                                } else {
                                    *retry_count
                                },
                            ))
                            .await
                    {
                        tracing::warn!(run_id = %ctx.run_id, %error, "failed to persist stream checkpoint");
                    }
                    // R2: tool calls finished before the drop → execute them now
                    // rather than re-inferring, even if the retry budget is spent.
                    if !completed.is_empty() {
                        return Ok(synthesized_tool_response(&combined_text, completed));
                    }
                }

                let retry_decision = bounded_retry_decision(
                    RETRY_ACTIVE_STATE,
                    retryable,
                    u32::try_from(attempt).unwrap_or(u32::MAX),
                    u32::try_from(policy.max_retries).unwrap_or(u32::MAX),
                );
                if retry_decision == RetryDecision::Retry {
                    // R1/R3/R4 collapse: continue from the combined text (empty ⇒
                    // clean restart); any still-open tool call is dropped.
                    prefix = combined_text;
                    *retry_count = retry_count.saturating_add(1);
                    model_read(
                        cancellation,
                        tokio::time::sleep(policy.delay_before_retry(&err, attempt)),
                    )
                    .await;
                    attempt += 1;
                } else {
                    return Err(err);
                }
            }
        }
    }
}

/// Forwards a step's streamed text chunks to the live stream as `OutputText`
/// events. Live progress only — the committed message comes from the returned
/// response, never these chunks (G10/G13).
pub(super) struct StreamDeltaSink<'a> {
    pub(super) context: &'a RuntimeRunContext,
    pub(super) run_id: &'a RunId,
    pub(super) thread_id: &'a ThreadId,
    pub(super) step: usize,
    pub(super) response: usize,
}

#[async_trait]
impl DeltaSink for StreamDeltaSink<'_> {
    async fn on_text(&self, chunk: &str) {
        emit_stream(
            self.context,
            StreamObservation::assistant_delta(
                self.run_id.clone(),
                self.thread_id.clone(),
                self.step,
                self.response,
                AgentEvent::Delta(Delta::TextDelta {
                    delta: chunk.to_string(),
                }),
            ),
        )
        .await;
    }

    async fn on_reasoning(&self, chunk: &str) {
        emit_stream(
            self.context,
            StreamObservation::assistant_delta(
                self.run_id.clone(),
                self.thread_id.clone(),
                self.step,
                self.response,
                AgentEvent::Delta(Delta::ReasoningDelta {
                    delta: chunk.to_string(),
                }),
            ),
        )
        .await;
    }

    async fn on_tool_call_delta(&self, call_id: &str, tool_id: &str, args_delta: &str) {
        emit_stream(
            self.context,
            StreamObservation::assistant_delta(
                self.run_id.clone(),
                self.thread_id.clone(),
                self.step,
                self.response,
                AgentEvent::Delta(Delta::ToolCallDelta {
                    id: call_id.to_string(),
                    name: tool_id.to_string(),
                    args_delta: args_delta.to_string(),
                }),
            ),
        )
        .await;
    }
}

/// The model-facing prompt appended after a confirmed partial when continuing an
/// interrupted stream, so the model resumes instead of regenerating. Same intent
/// as the in-place `MaxTokens` continuation, but for a transient mid-stream drop.
const STREAM_CONTINUATION_PROMPT: &str = "Your previous response was interrupted \
    mid-stream. Continue from exactly where you left off, without repeating any \
    text you already wrote.";

/// The current attempt's captured partial: streamed text plus any tool calls
/// seen (each with its accumulated-so-far raw argument text, last-wins by id).
struct Snapshot {
    text: String,
    tools: Vec<PartialToolCall>,
}

/// Wraps the live `DeltaSink` to also capture the current attempt's streamed
/// partial, so a mid-stream interruption can be *continued from it* rather than
/// regenerated from scratch (R1–R3 recovery). Text and tool progress are
/// forwarded to `inner` unchanged, so the live stream is untouched. Tool
/// arguments arrive as de-accumulated suffix fragments (see `on_tool_call_delta`)
/// which are appended into raw JSON text; whether it parses later decides
/// completed-vs-in-flight.
struct ContinuationSink<'a> {
    inner: &'a dyn DeltaSink,
    text: std::sync::Mutex<String>,
    tools: std::sync::Mutex<Vec<PartialToolCall>>,
}

impl<'a> ContinuationSink<'a> {
    fn new(inner: &'a dyn DeltaSink) -> Self {
        Self {
            inner,
            text: std::sync::Mutex::new(String::new()),
            tools: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The current attempt's captured partial.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.lock().expect("continuation buffer").clone(),
            tools: self.tools.lock().expect("continuation tools").clone(),
        }
    }

    /// Reset per-attempt capture before a fresh attempt streams.
    fn reset(&self) {
        self.text.lock().expect("continuation buffer").clear();
        self.tools.lock().expect("continuation tools").clear();
    }
}

#[async_trait]
impl DeltaSink for ContinuationSink<'_> {
    async fn on_text(&self, chunk: &str) {
        // Guard drops at the statement end, never held across the await below.
        self.text
            .lock()
            .expect("continuation buffer")
            .push_str(chunk);
        self.inner.on_text(chunk).await;
    }

    async fn on_reasoning(&self, chunk: &str) {
        self.inner.on_reasoning(chunk).await;
    }

    async fn on_tool_call_delta(&self, call_id: &str, tool_id: &str, args_delta: &str) {
        // The provider adapter now hands a suffix `args_delta`, so accumulate them
        // per call id to rebuild the running raw JSON — a later parse decides whether
        // the call had finished when a mid-stream drop lands mid-arguments.
        {
            let mut tools = self.tools.lock().expect("continuation tools");
            match tools.iter_mut().find(|t| t.call_id == call_id) {
                Some(existing) => {
                    existing.tool_id = tool_id.to_string();
                    existing.raw_arguments.push_str(args_delta);
                }
                None => tools.push(PartialToolCall {
                    call_id: call_id.to_string(),
                    tool_id: tool_id.to_string(),
                    raw_arguments: args_delta.to_string(),
                }),
            }
        }
        self.inner
            .on_tool_call_delta(call_id, tool_id, args_delta)
            .await;
    }
}

/// The tool calls whose accumulated arguments parse as complete JSON — i.e. the
/// model finished emitting them before the drop, so they can be executed as-is
/// (R2). A call still in flight (unparseable / empty-named args) is excluded.
fn parse_completed(tools: &[PartialToolCall]) -> Vec<ToolCall> {
    tools
        .iter()
        .filter(|tool| !tool.tool_id.is_empty())
        .filter_map(|tool| {
            serde_json::from_str::<serde_json::Value>(&tool.raw_arguments)
                .ok()
                .map(|arguments| ToolCall {
                    call_id: tool.call_id.clone(),
                    tool_id: tool.tool_id.clone(),
                    arguments,
                })
        })
        .collect()
}

/// Synthesize the step the model had produced when a drop interrupted it after
/// its tool calls were complete (R2): the salvaged text followed by the completed
/// tool-use blocks, stopped for tool use. The engine's tool loop runs these
/// without another model round-trip.
fn synthesized_tool_response(text: &str, calls: Vec<ToolCall>) -> ChatResponse {
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(ContentBlock::text(text.to_string()));
    }
    for call in calls {
        blocks.push(ContentBlock::tool_use(
            call.call_id,
            call.tool_id,
            call.arguments,
        ));
    }
    ChatResponse {
        output: AssistantOutput::from_blocks(blocks),
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
    }
}

/// Per-run wiring for durable interrupted-stream checkpoints (Phase 3), present
/// only when the run context supplies a `StreamCheckpointStore`. Holds the keys a
/// flush needs; the store handle is borrowed for the run's duration.
pub(super) struct CheckpointCtx<'a> {
    pub(super) store: &'a dyn StreamCheckpointStore,
    pub(super) run_id: String,
    pub(super) thread_id: String,
    pub(super) model: String,
}

impl CheckpointCtx<'_> {
    fn checkpoint(
        &self,
        text: String,
        tools: Vec<PartialToolCall>,
        retry_count: u32,
    ) -> StreamCheckpoint {
        StreamCheckpoint {
            run_id: self.run_id.clone(),
            thread_id: self.thread_id.clone(),
            model: self.model.clone(),
            partial_text: text,
            partial_tools: tools,
            retry_count,
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::should_clear_spent_checkpoint;

    #[kani::proof]
    fn checkpoint_clear_requires_unvetoed_terminal_completion() {
        let cancelled = kani::any::<bool>();
        let journal_failed = kani::any::<bool>();
        let can_acknowledge_return = kani::any::<bool>();

        assert_eq!(
            should_clear_spent_checkpoint(cancelled, journal_failed, can_acknowledge_return),
            !cancelled && !journal_failed && can_acknowledge_return
        );
    }
}
