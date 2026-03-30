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

    struct StreamingMockExecutor {
        events: std::sync::Mutex<Option<Vec<Result<LlmStreamEvent, InferenceExecutionError>>>>,
    }

    impl StreamingMockExecutor {
        fn new(events: Vec<Result<LlmStreamEvent, InferenceExecutionError>>) -> Self {
            Self {
                events: std::sync::Mutex::new(Some(events)),
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
            let events = self.events.lock().unwrap().take().unwrap_or_default();
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

    fn make_request() -> InferenceRequest {
        InferenceRequest {
            model: "test-model".into(),
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

        let result = execute_streaming(
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

        execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = execute_streaming(
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

        execute_streaming(
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

        let result = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = execute_streaming(
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

        let result = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot)
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

        let result = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot)
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
        let agent = make_agent(vec![
            Ok(LlmStreamEvent::TextDelta("before error".into())),
            Err(InferenceExecutionError::Provider("rate limited".into())),
        ]);
        let sink = VecEventSink::new();
        let mut it = 0u64;
        let mut ot = 0u64;

        let err = execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot)
            .await
            .unwrap_err();

        match err {
            AgentLoopError::InferenceFailed(msg) => {
                assert!(msg.contains("rate limited"));
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

        execute_streaming(&agent, make_request(), &sink, None, &mut it, &mut ot)
            .await
            .unwrap();

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallReady { id, name, .. } if id == "tc1" && name == "calculator"
        )));
    }
}
