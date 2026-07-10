//! The model-inference call seam: retry + circuit-breaker + streaming +
//! mid-stream recovery (R1–R4) + durable checkpoint flush, plus the OTel GenAI
//! `chat` span. Split out of `engine/mod.rs` (the step-loop driver) so the
//! "call the provider" responsibility is isolated from "drive the loop" and the
//! module stays within the file-length limit. Items are shared through
//! `use super::*` (same-crate privates included).

use super::*;

/// Rebuild a request that carries the confirmed partial as an assistant prefix
/// followed by a continuation prompt, so the model continues rather than
/// regenerates. Request-only: these two messages are never committed to the
/// transcript — a transient interruption is not a real turn boundary.
fn continuation_request(request: &ChatRequest, prefix: &str) -> ChatRequest {
    let mut messages = request.messages.clone();
    messages.push(ChatMessage {
        role: ChatRole::Assistant,
        content: vec![ContentBlock::text(prefix.to_string())],
    });
    messages.push(ChatMessage {
        role: ChatRole::User,
        content: vec![ContentBlock::text(STREAM_CONTINUATION_PROMPT.to_string())],
    });
    ChatRequest {
        messages,
        ..request.clone()
    }
}

/// Prepend the confirmed partial `prefix` onto a continued response so the
/// committed turn is the whole text. An empty prefix returns the response
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
/// A stream cut off mid-flight is recovered from its partial rather than
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
) -> std::result::Result<ChatResponse, awaken_runtime_contract::llm::Error> {
    let span = tracing::Span::current();
    // OTel GenAI span name is the templated `{operation} {model}` (SHOULD).
    span.record(
        "otel.name",
        format!("chat {}", request.model_binding.model_ref).as_str(),
    );
    // Content telemetry (ADR-0050): gated + scrubbed; before the request moves.
    content::emit_content(
        capture,
        content_sink,
        awaken_runtime_contract::ContentKind::InputMessages,
        "gen_ai.input.messages",
        &content::render_chat_messages(&request.messages),
    )
    .await;
    let result =
        infer_with_retry_inner(llm, request, policy, breaker, sink, checkpoint, resume).await;
    // Any return means recovery concluded in-process, so the checkpoint (if any)
    // is spent. It survives only a crash *before* this line — mid-recovery —
    // which is exactly the cross-process window Phase 3 guards.
    if let Some(ctx) = checkpoint {
        ctx.store.delete(&ctx.run_id).await;
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
            content::emit_content(
                capture,
                content_sink,
                awaken_runtime_contract::ContentKind::OutputMessages,
                "gen_ai.output.messages",
                &response.output.text_content(),
            )
            .await;
        }
        // OTel: on a failed inference, tag `error.type` (the neutral taxonomy code)
        // and mark the span status ERROR so the trace surfaces the failure.
        Err(err) => {
            span.record("error.type", err.code());
            span.record("otel.status_code", "ERROR");
        }
    }
    result
}

async fn infer_with_retry_inner(
    llm: &std::sync::Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
    request: ChatRequest,
    policy: &crate::retry::LlmRetryPolicy,
    breaker: &crate::circuit_breaker::CircuitBreaker,
    sink: &dyn DeltaSink,
    checkpoint: Option<&CheckpointCtx<'_>>,
    resume: Option<StreamCheckpoint>,
) -> std::result::Result<ChatResponse, awaken_runtime_contract::llm::Error> {
    let model = request.model_binding.model_ref.clone();
    let sink = ContinuationSink::new(sink);
    // Text confirmed from prior interrupted attempts; grows as continuation
    // proceeds. Empty means "start (or restart) clean".
    let mut prefix = String::new();

    // Cross-process resume: reproduce in-process the plan a fresh interruption
    // would have taken from this persisted partial.
    if let Some(cp) = resume {
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
        if let Err(reason) = breaker.check(&model) {
            return Err(awaken_runtime_contract::llm::Error::Provider(reason));
        }
        let attempt_request = if prefix.is_empty() {
            request.clone()
        } else {
            continuation_request(&request, &prefix)
        };
        sink.reset();
        match llm.infer_streaming(attempt_request, &sink).await {
            Ok(response) => {
                breaker.record_success(&model);
                return Ok(stitch_prefix(response, &prefix));
            }
            Err(err) => {
                // Only retryable errors speak to provider health; the counted
                // set must stay exactly `is_retryable`, or permanent faults
                // (bad key, overlong prompt) would trip the breaker.
                let retryable = err.is_retryable();
                if retryable {
                    breaker.record_failure(&model);
                }
                let snapshot = sink.snapshot();
                // The whole text so far: prior confirmed prefix + this attempt.
                let combined_text = format!("{prefix}{}", snapshot.text);

                if retryable {
                    // Boundary flush (Phase 3): persist the whole in-flight partial
                    // so a crash during recovery resumes here. Best-effort — a
                    // store fault must never turn a recoverable blip into a failure.
                    if let Some(ctx) = checkpoint {
                        ctx.store
                            .put(ctx.checkpoint(combined_text.clone(), snapshot.tools.clone()))
                            .await;
                    }
                    // R2: tool calls finished before the drop → execute them now
                    // rather than re-inferring, even if the retry budget is spent.
                    let completed = parse_completed(&snapshot.tools);
                    if !completed.is_empty() {
                        return Ok(synthesized_tool_response(&combined_text, completed));
                    }
                }

                if retryable && attempt < policy.max_retries {
                    // R1/R3/R4 collapse: continue from the combined text (empty ⇒
                    // clean restart); any still-open tool call is dropped.
                    prefix = combined_text;
                    tokio::time::sleep(policy.delay_before_retry(&err, attempt)).await;
                    attempt += 1;
                } else {
                    return Err(err);
                }
            }
        }
    }
}
