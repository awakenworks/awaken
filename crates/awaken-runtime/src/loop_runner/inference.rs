//! LLM inference execution and context compaction.

use std::sync::Arc;
use std::time::Duration;

use crate::cancellation::CancellationToken;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::executor::{
    InFlightTool, InferenceExecutionError, InferenceRequest, InterruptCause, InterruptSnapshot,
    LlmStreamEvent, RecoveryPlan,
};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::message::{Message, ToolCall};
use futures::StreamExt;

use super::AgentLoopError;
use crate::engine::retry::LlmRetryPolicy;
use crate::registry::ResolvedAgent;

/// Continuation prompt injected after a mid-stream interruption to nudge
/// the model to pick up where its partial response left off. Intentionally
/// short — long prompts dilute the assistant prefix context.
const CONTINUATION_PROMPT: &str =
    "Your previous response was interrupted mid-stream. Continue from where you left off.";

/// Execute LLM inference with streaming, emitting delta events via sink.
///
/// Consumes the token stream from `execute_stream()`, forwards deltas to sink,
/// and collects the final `StreamResult`.
///
/// Supports mid-stream cancellation: if the `CancellationToken` is signalled while
/// waiting for the next token, the stream is dropped and the partially accumulated
/// result is returned with `StopReason::EndTurn` (graceful cancel — no error).
pub(super) async fn execute_streaming(
    agent: &ResolvedAgent,
    mut request: InferenceRequest,
    sink: &dyn EventSink,
    cancellation_token: Option<&CancellationToken>,
    total_input_tokens: &mut u64,
    total_output_tokens: &mut u64,
) -> Result<(StreamResult, Option<InFlightTool>), AgentLoopError> {
    let policy = stream_retry_policy_for(agent);
    let idle_timeout = idle_timeout_for(&request, &policy);
    let max_retries = policy.max_stream_retries;
    let mut attempt: u32 = 0;

    loop {
        let outcome = drive_one_stream(
            agent,
            request.clone(),
            sink,
            cancellation_token,
            total_input_tokens,
            total_output_tokens,
            idle_timeout,
        )
        .await;

        match outcome {
            DriveOutcome::Completed(result) | DriveOutcome::Cancelled(result) => {
                return Ok((result, None));
            }
            DriveOutcome::Error(err) => return Err(err),
            DriveOutcome::Interrupted { cause, snapshot } => {
                if attempt >= max_retries {
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
                        return Ok((result, hint));
                    }
                    RecoveryOutcome::RetryAfterPlan => {
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
                        continue;
                    }
                }
            }
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
async fn drive_one_stream(
    agent: &ResolvedAgent,
    request: InferenceRequest,
    sink: &dyn EventSink,
    cancellation_token: Option<&CancellationToken>,
    total_input_tokens: &mut u64,
    total_output_tokens: &mut u64,
    idle_timeout: Duration,
) -> DriveOutcome {
    let mut token_stream = match agent.llm_executor.execute_stream(request).await {
        Ok(s) => s,
        Err(err) => {
            // `err` here comes from the executor (including `RetryingExecutor`)
            // and has already exhausted its own retries. Surface it.
            return DriveOutcome::Error(AgentLoopError::from(err));
        }
    };

    let mut acc = StreamingAccumulator::default();

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
                // Idle stall — no delta in `idle_timeout`.
                let snapshot = acc.interrupt_snapshot();
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
                // Mid-stream transport error. Classify and surface.
                let snapshot = acc.interrupt_snapshot();
                match classify_mid_stream(&err) {
                    Some(cause) => {
                        tracing::debug!(
                            cause = %cause,
                            bytes_received = snapshot.bytes_received,
                            "mid-stream error captured, entering recovery"
                        );
                        return DriveOutcome::Interrupted { cause, snapshot };
                    }
                    None => return DriveOutcome::Error(AgentLoopError::from(err)),
                }
            }
        };

        match event {
            LlmStreamEvent::TextDelta(delta) => {
                acc.current_text.push_str(&delta);
                sink.emit(AgentEvent::TextDelta { delta }).await;
            }
            LlmStreamEvent::ReasoningDelta(delta) => {
                sink.emit(AgentEvent::ReasoningDelta { delta }).await;
            }
            LlmStreamEvent::ToolCallStart { id, name } => {
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
    }

    // Stream drained cleanly (or cancelled): finalize.
    let result = acc.finalize(sink).await;
    if acc.cancelled {
        DriveOutcome::Cancelled(result)
    } else {
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
    /// state. Preserves text (may be empty), marks tool calls with valid
    /// JSON as completed and the most-recent unparseable one as in-flight.
    fn interrupt_snapshot(&self) -> InterruptSnapshot {
        let mut completed: Vec<ToolCall> = Vec::new();
        let mut in_flight: Option<InFlightTool> = None;

        for id in &self.tool_order {
            let args_json = self.current_tool_args.get(id).cloned().unwrap_or_default();
            let name = self.tool_names.get(id).cloned().unwrap_or_default();

            if name.is_empty() {
                in_flight = Some(InFlightTool {
                    id: id.clone(),
                    name: String::new(),
                    partial_args: args_json,
                });
                continue;
            }

            match serde_json::from_str::<serde_json::Value>(&args_json) {
                Ok(arguments) if !(arguments.is_null() && !args_json.is_empty()) => {
                    completed.push(ToolCall::new(id.clone(), name, arguments));
                }
                _ => {
                    in_flight = Some(InFlightTool {
                        id: id.clone(),
                        name,
                        partial_args: args_json,
                    });
                }
            }
        }

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

        InterruptSnapshot {
            text,
            completed_tool_calls: completed,
            in_flight_tool: in_flight,
            bytes_received: self.bytes_received,
        }
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
        InferenceExecutionError::Provider(_)
        | InferenceExecutionError::Timeout(_)
        | InferenceExecutionError::RateLimited { .. }
        | InferenceExecutionError::Overloaded { .. } => Some(InterruptCause::ConnectionReset),

        // Already-classified stream interruption — preserve cause.
        InferenceExecutionError::StreamInterrupted { cause, .. } => Some(cause.clone()),

        // Permanent / lifecycle — surface to caller, not a mid-stream retry.
        _ => None,
    }
}

/// Fetch the active retry policy. Falls back to defaults for agents that
/// do not configure one. The agent-level override plumbing lives in
/// `engine::retry::RetryConfigKey`; for now, treat missing config as
/// "use defaults".
fn stream_retry_policy_for(_agent: &ResolvedAgent) -> LlmRetryPolicy {
    LlmRetryPolicy::default()
}

/// Model-aware idle-stall threshold. Thinking / reasoning models receive
/// a 2× window to accommodate long silent reasoning phases.
fn idle_timeout_for(request: &InferenceRequest, policy: &LlmRetryPolicy) -> Duration {
    let base = Duration::from_secs(policy.stream_idle_timeout_secs);
    let model = request.upstream_model.as_str();
    let name_hits = model.contains("thinking")
        || model.contains("reasoning")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4");
    let options_hits = request
        .overrides
        .as_ref()
        .and_then(|o| o.reasoning_effort.as_ref())
        .is_some();
    if name_hits || options_hits {
        base * 2
    } else {
        base
    }
}

fn stream_retry_backoff(cause: &InterruptCause, attempt: u32, policy: &LlmRetryPolicy) -> Duration {
    // Mid-stream retries use the normal backoff; Overloaded-style
    // surges propagate through `RetryingExecutor` on stream open, not
    // here. For idle stalls, use a short delay to probe quickly.
    match cause {
        InterruptCause::IdleStall => Duration::from_millis(200),
        _ => policy.delay_before_retry(
            &InferenceExecutionError::Provider("mid-stream".into()),
            attempt,
        ),
    }
}

/// Compact messages using the configured ContextSummarizer.
///
/// Finds a safe compaction boundary, renders messages as transcript (filtering
/// Internal messages), extracts any previous summary for cumulative updates,
/// calls the summarizer, and replaces old messages with the summary.
///
/// Skips compaction if the estimated token savings are below `MIN_COMPACTION_GAIN_TOKENS`.
pub(super) async fn compact_with_llm(
    agent: &ResolvedAgent,
    messages: &mut Vec<Arc<Message>>,
    policy: &awaken_contract::contract::inference::ContextWindowPolicy,
) -> Result<(), AgentLoopError> {
    use crate::context::{
        MIN_COMPACTION_GAIN_TOKENS, extract_previous_summary, find_compaction_boundary,
        render_transcript,
    };

    let summarizer = match agent.context_summarizer {
        Some(ref s) => s,
        None => return Ok(()),
    };

    if messages.len() < 2 {
        return Ok(());
    }

    let keep_suffix = policy.compaction_raw_suffix_messages.min(messages.len());
    let search_end = messages.len().saturating_sub(keep_suffix);
    if search_end < 2 {
        return Ok(());
    }

    let boundary = match find_compaction_boundary(messages, 0, search_end) {
        Some(b) => b,
        None => return Ok(()),
    };

    // Check minimum gain threshold
    let compactable_tokens: usize = messages[..=boundary]
        .iter()
        .map(|message| awaken_contract::contract::transform::estimate_message_tokens(message))
        .sum();
    if compactable_tokens < MIN_COMPACTION_GAIN_TOKENS {
        return Ok(());
    }

    // Render transcript (excludes Internal messages)
    let transcript = render_transcript(&messages[..=boundary]);
    if transcript.is_empty() {
        return Ok(());
    }

    // Extract previous summary for cumulative update
    let previous_summary = extract_previous_summary(messages);

    let summary_text = summarizer
        .summarize(
            &transcript,
            previous_summary.as_deref(),
            agent.llm_executor.as_ref(),
        )
        .await
        .map_err(|e| AgentLoopError::InferenceFailed(format!("compaction failed: {e}")))?;

    // Replace messages up to boundary with the summary
    let post_tokens =
        awaken_contract::contract::transform::estimate_tokens(&messages[boundary + 1..]);
    messages.drain(..=boundary);
    messages.insert(
        0,
        Arc::new(Message::internal_system(format!(
            "<conversation-summary>\n{summary_text}\n</conversation-summary>"
        ))),
    );

    tracing::info!(
        pre_tokens = compactable_tokens,
        post_tokens,
        boundary,
        "compaction_complete"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancellation::CancellationToken;
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

    // -- Streaming LLM executor that yields explicit stream events --

    /// Mock LLM executor that yields a fixed scripted event sequence on
    /// EVERY call to `execute_stream`. Cloning the script per attempt means
    /// stream-level retries in `execute_streaming` observe the same
    /// behavior across attempts — useful when asserting retry budgets.
    struct StreamingMockExecutor {
        script: Vec<ClonedEvent>,
    }

    /// Serializable-as-needed twin of the scripted events. `LlmStreamEvent`
    /// itself is Clone via Copy/String fields; `InferenceExecutionError` is
    /// now Clone as of the Phase-A refactor — so a straightforward clone
    /// works, but we normalize to owned values here for clarity.
    #[derive(Clone)]
    struct ClonedEvent(Result<LlmStreamEvent, InferenceExecutionError>);

    impl StreamingMockExecutor {
        fn new(events: Vec<Result<LlmStreamEvent, InferenceExecutionError>>) -> Self {
            Self {
                script: events.into_iter().map(ClonedEvent).collect(),
            }
        }
    }

    #[async_trait]
    impl awaken_contract::contract::executor::LlmExecutor for StreamingMockExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: vec![],
                tool_calls: vec![],
                usage: None,
                stop_reason: None,
                has_incomplete_tool_calls: false,
            })
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
            let events: Vec<Result<LlmStreamEvent, InferenceExecutionError>> =
                self.script.iter().map(|e| e.0.clone()).collect();
            Box::pin(async move { Ok(Box::pin(futures::stream::iter(events)) as InferenceStream) })
        }

        fn name(&self) -> &str {
            "streaming-mock"
        }
    }

    fn make_agent(events: Vec<Result<LlmStreamEvent, InferenceExecutionError>>) -> ResolvedAgent {
        ResolvedAgent::new(
            "test-agent",
            "test-model",
            "system prompt",
            Arc::new(StreamingMockExecutor::new(events)),
        )
    }

    /// Thin adapter that discards the in-flight tool hint. Used by
    /// legacy tests that only care about the `StreamResult`; new tests
    /// that exercise the hint path (Phase E) call `execute_streaming`
    /// directly and destructure the tuple.
    async fn stream_only(
        agent: &ResolvedAgent,
        request: InferenceRequest,
        sink: &dyn EventSink,
        cancellation_token: Option<&CancellationToken>,
        total_input_tokens: &mut u64,
        total_output_tokens: &mut u64,
    ) -> Result<StreamResult, AgentLoopError> {
        execute_streaming(
            agent,
            request,
            sink,
            cancellation_token,
            total_input_tokens,
            total_output_tokens,
        )
        .await
        .map(|(result, _hint)| result)
    }

    fn make_request() -> InferenceRequest {
        InferenceRequest {
            upstream_model: "test-model".into(),
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

        let result = stream_only(
            &agent,
            make_request(),
            &sink,
            None,
            &mut input_tokens,
            &mut output_tokens,
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

        stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = stream_only(
            &agent,
            make_request(),
            &sink,
            None,
            &mut input_tokens,
            &mut output_tokens,
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

        stream_only(
            &agent,
            make_request(),
            &sink,
            None,
            &mut input_tokens,
            &mut output_tokens,
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

        let result = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = stream_only(
            &agent,
            make_request(),
            &sink,
            Some(&token),
            &mut it,
            &mut ot,
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

        let result = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let err = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let err = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let err = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let err = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let err = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let err = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = stream_only(
            &agent,
            make_request(),
            &sink,
            Some(&token),
            &mut it,
            &mut ot,
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

        let err = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

    /// Scripted streaming executor keyed by attempt number. On the Nth call
    /// to `execute_stream`, yields `scripts[min(N, scripts.len()-1)]` so
    /// short scripts naturally repeat the last attempt's script forever.
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

    fn agent_with(exec: Arc<ScriptedPerAttemptExecutor>) -> ResolvedAgent {
        ResolvedAgent::new("test-agent", "test-model", "system prompt", exec)
    }

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

        let result = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let err = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot)
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
        let exec_fut = stream_only(&agent, make_request(), &sink, None, &mut it, &mut ot);
        let drive = async {
            // Wait for the first TextDelta to be emitted, then advance
            // past the idle threshold to trigger the stall.
            tokio::time::sleep(Duration::from_millis(1)).await;
            tokio::time::advance(Duration::from_secs(70)).await;
        };

        let (result, ()) = tokio::join!(exec_fut, drive);
        let result = result.expect("idle-stall should recover");
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
}
