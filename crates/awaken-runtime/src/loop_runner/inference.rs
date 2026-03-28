//! LLM inference execution and context compaction.

use std::sync::Arc;

use crate::cancellation::CancellationToken;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::executor::{InferenceRequest, LlmStreamEvent};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::message::{Message, ToolCall};
use futures::StreamExt;

use super::AgentLoopError;
use crate::registry::ResolvedAgent;

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
    request: InferenceRequest,
    sink: &dyn EventSink,
    cancellation_token: Option<&CancellationToken>,
    total_input_tokens: &mut u64,
    total_output_tokens: &mut u64,
) -> Result<StreamResult, AgentLoopError> {
    use awaken_contract::contract::content::ContentBlock;

    let mut token_stream = agent.llm_executor.execute_stream(request).await?;

    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage: Option<TokenUsage> = None;
    let mut stop_reason: Option<StopReason> = None;
    let mut current_text = String::new();
    let mut current_tool_args: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut tool_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    // Track insertion order so tool_calls preserves the LLM's declared order.
    let mut tool_order: Vec<String> = Vec::new();
    let mut cancelled = false;

    loop {
        let event = if let Some(token) = cancellation_token {
            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    cancelled = true;
                    break;
                }
                next = token_stream.next() => next,
            }
        } else {
            token_stream.next().await
        };

        let Some(event_result) = event else {
            break; // stream ended
        };

        let event = event_result?;

        match event {
            LlmStreamEvent::TextDelta(delta) => {
                current_text.push_str(&delta);
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
                tool_names.insert(id.clone(), name);
                current_tool_args.insert(id.clone(), String::new());
                tool_order.push(id);
            }
            LlmStreamEvent::ToolCallDelta { id, args_delta } => {
                if let Some(buf) = current_tool_args.get_mut(&id) {
                    buf.push_str(&args_delta);
                }
                sink.emit(AgentEvent::ToolCallDelta { id, args_delta })
                    .await;
            }
            LlmStreamEvent::ContentBlockStop => {
                if !current_text.is_empty() {
                    content_blocks.push(ContentBlock::text(std::mem::take(&mut current_text)));
                }
            }
            LlmStreamEvent::Usage(u) => {
                if let Some(v) = u.prompt_tokens {
                    *total_input_tokens = total_input_tokens.saturating_add(v.max(0) as u64);
                }
                if let Some(v) = u.completion_tokens {
                    *total_output_tokens = total_output_tokens.saturating_add(v.max(0) as u64);
                }
                usage = Some(u);
            }
            LlmStreamEvent::Stop(reason) => {
                stop_reason = Some(reason);
            }
        }
    }

    // Flush remaining text
    if !current_text.is_empty() {
        content_blocks.push(ContentBlock::text(current_text));
    }

    // Collect tool calls from accumulated args in LLM-declared order (drop incomplete on cancel)
    let mut has_incomplete_tool_calls = false;
    if !cancelled {
        for id in &tool_order {
            let args_json = current_tool_args.get(id).cloned().unwrap_or_default();
            let name = tool_names.get(id).cloned().unwrap_or_default();
            let arguments = serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
            if arguments.is_null() && !args_json.is_empty() {
                has_incomplete_tool_calls = true;
                continue; // truncated JSON, skip
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

    Ok(StreamResult {
        content: content_blocks,
        tool_calls,
        usage,
        stop_reason: if cancelled {
            Some(StopReason::EndTurn)
        } else {
            stop_reason
        },
        has_incomplete_tool_calls,
    })
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
        .map(|m| awaken_contract::contract::transform::estimate_message_tokens(m))
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
        awaken_contract::contract::transform::estimate_tokens_arc(&messages[boundary + 1..]);
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
