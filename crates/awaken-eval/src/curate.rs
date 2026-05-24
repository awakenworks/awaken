//! Trace → `provider_script` converter (ADR-0032 D5).
//!
//! Reads a `Vec<MetricsEvent>` (typically produced by a `TraceStore::read`)
//! and reconstructs the sequence of [`ProviderScriptEvent`]s that would,
//! when replayed through [`ScriptedLlmExecutor`], reproduce the same agent
//! behaviour.
//!
//! ## Prerequisites
//!
//! The originating run **must** have enabled
//! `ObservabilityPlugin::with_content_capture(Enabled)` — the converter
//! reads `GenAISpan::response_content` / `response_tool_calls`, which are
//! `None` by default. A trace without content capture produces
//! [`CurateError::MissingContent`].
//!
//! ## Recovering `user_input`
//!
//! When the originating run also had `ContentCapture::Enabled`, the
//! first inference span carries the full `request_messages` history.
//! The converter pulls the first user message out of that and surfaces
//! it on [`TraceConversion::user_input`]. The CLI / server fall back to
//! this when the operator doesn't supply `--user-input` explicitly.
//!
//! When `request_messages` isn't captured (legacy traces, or capture
//! disabled), `user_input` is `None` and the caller must supply it.

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::inference::{StopReason, TokenUsage};
use awaken_contract::contract::message::{Message, Role, ToolCall};
use awaken_ext_observability::{GenAISpan, MetricsEvent};
use awaken_runtime::engine::ProviderScriptEvent;
use thiserror::Error;

/// What a successful conversion returns.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceConversion {
    /// The reconstructed scripted upstream — one event per inference span
    /// in trace order. `ScriptedLlmExecutor::new(provider_script)` replays
    /// the original run deterministically.
    pub provider_script: Vec<ProviderScriptEvent>,
    /// `GenAISpan::model` of the first inference span in the trace. Used
    /// to populate `Fixture::source_model_id` so the model-guard catches
    /// drift when the fixture is later replayed against a different
    /// upstream.
    pub source_model_id: Option<String>,
    /// Text of the first user message recovered from the trace's
    /// `request_messages` capture (first inference span only). `None`
    /// when the originating run didn't enable ContentCapture, when no
    /// user message appears in the captured history, or when the
    /// message text was non-string content (image, document). Callers
    /// fall back to operator-supplied input in that case.
    pub user_input: Option<String>,
}

/// Errors raised by [`trace_to_provider_script`].
#[derive(Debug, Error)]
pub enum CurateError {
    /// The trace contained zero `MetricsEvent::Inference` records. Replay
    /// would have nothing to script — the trace is either truncated or
    /// for a run that errored before invoking the LLM.
    #[error("trace contained no inference spans — nothing to convert")]
    NoInferences,
    /// An inference span had neither `response_content` nor
    /// `response_tool_calls` populated. Almost always means the originating
    /// run was recorded without `ContentCapture::Enabled`.
    #[error(
        "inference span at step {step} has no captured response_content nor tool_calls — \
         enable ObservabilityPlugin::with_content_capture(Enabled) on the originating run"
    )]
    MissingContent { step: u32 },
    /// `response_content` was present but its shape didn't decode to
    /// `Vec<ContentBlock>`. The capture format changed under us — surface
    /// the JSON error so the caller can pin a contract version.
    #[error("response_content for step {step} could not be decoded as Vec<ContentBlock>")]
    ContentDecode {
        step: u32,
        #[source]
        source: serde_json::Error,
    },
    /// `response_tool_calls` was present but its shape didn't decode to
    /// `Vec<ToolCall>`. See [`Self::ContentDecode`].
    #[error("response_tool_calls for step {step} could not be decoded as Vec<ToolCall>")]
    ToolCallsDecode {
        step: u32,
        #[source]
        source: serde_json::Error,
    },
    /// A `response_tool_calls` field was present but the decoded list was
    /// empty. `StopReason::ToolUse` with zero calls is not representable
    /// as a `ProviderScriptEvent::ToolCall` — the trace is malformed.
    #[error("inference span at step {step} captured an empty response_tool_calls list")]
    EmptyToolCalls { step: u32 },
    /// The scripted executor can currently return only one tool call per
    /// upstream turn. Refuse to silently drop parallel calls.
    #[error(
        "inference span at step {step} captured {count} tool calls; \
         provider_script currently supports one tool call per turn"
    )]
    MultipleToolCalls { step: u32, count: usize },
    /// A turn carried both tool calls and assistant content. That shape is
    /// valid for some providers, but `ProviderScriptEvent::ToolCall`
    /// cannot preserve the assistant text today.
    #[error(
        "inference span at step {step} captured assistant content alongside tool calls; \
         provider_script cannot represent mixed content/tool-use turns"
    )]
    MixedToolCallContent { step: u32 },
    /// A chat response included a content block the scripted executor
    /// cannot replay as `ChatResponse { content: String }`.
    #[error(
        "inference span at step {step} captured unsupported {block_type} content; \
         provider_script only supports text chat responses today"
    )]
    UnsupportedContentBlock { step: u32, block_type: String },
}

/// Walk the trace's events in order, reconstructing the scripted upstream.
///
/// Only `MetricsEvent::Inference` records contribute to the script:
/// `ScriptedLlmExecutor` models the upstream LLM, and tool spans
/// (`MetricsEvent::Tool`) describe runtime-side execution that the
/// replayer drives itself. Skipping tool spans is therefore correct, not
/// a loss.
pub fn trace_to_provider_script(events: &[MetricsEvent]) -> Result<TraceConversion, CurateError> {
    let mut provider_script = Vec::new();
    let mut source_model_id: Option<String> = None;
    let mut user_input: Option<String> = None;

    for event in events {
        if let MetricsEvent::Inference(span) = event {
            if source_model_id.is_none() && !span.model.is_empty() {
                source_model_id = Some(span.model.clone());
            }
            if user_input.is_none() {
                user_input = latest_user_text(span);
            }
            provider_script.push(inference_to_script(span)?);
        }
    }

    if provider_script.is_empty() {
        return Err(CurateError::NoInferences);
    }

    Ok(TraceConversion {
        provider_script,
        source_model_id,
        user_input,
    })
}

/// Pull the *most recent* user message's text out of a span's captured
/// `request_messages` history. For a fresh thread this is also the only
/// user message; for a continuation thread it's the message that
/// triggered this run (not the original thread starter — that one is
/// the user_input we'd want for the thread's *first* eval fixture, not
/// for a fixture covering this specific turn).
///
/// Returns `None` if the field is absent (capture disabled or not the
/// first span), if no user message exists, or if the user message's
/// content is non-text (images, documents can't fit a plain
/// `Fixture.user_input` string).
fn latest_user_text(span: &GenAISpan) -> Option<String> {
    let messages_value = span.request_messages.as_ref()?;
    let messages: Vec<Message> = serde_json::from_value(messages_value.clone()).ok()?;
    for message in messages.into_iter().rev() {
        if !matches!(message.role, Role::User) {
            continue;
        }
        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

fn inference_to_script(span: &GenAISpan) -> Result<ProviderScriptEvent, CurateError> {
    let step = span.step_index.unwrap_or(0);

    // Error path: the AfterInferenceHook skips content capture when the
    // run errored, so `error_type` set + content fields None is the
    // expected shape. Surface it as a scripted Error event with the
    // recorded variant; the upstream message text is not persisted.
    if let Some(error_type) = &span.error_type {
        return Ok(ProviderScriptEvent::Error {
            error_type: error_type.clone(),
            message: String::new(),
        });
    }

    let tokens = build_tokens(span);

    if let Some(tool_calls_value) = &span.response_tool_calls {
        let tool_calls: Vec<ToolCall> = serde_json::from_value(tool_calls_value.clone())
            .map_err(|source| CurateError::ToolCallsDecode { step, source })?;
        if tool_calls.is_empty() {
            return Err(CurateError::EmptyToolCalls { step });
        }
        if tool_calls.len() > 1 {
            return Err(CurateError::MultipleToolCalls {
                step,
                count: tool_calls.len(),
            });
        }
        if let Some(content_value) = &span.response_content {
            let blocks: Vec<ContentBlock> = serde_json::from_value(content_value.clone())
                .map_err(|source| CurateError::ContentDecode { step, source })?;
            if !blocks.is_empty() {
                return Err(CurateError::MixedToolCallContent { step });
            }
        }
        let first = tool_calls
            .into_iter()
            .next()
            .expect("non-empty checked above");
        return Ok(ProviderScriptEvent::ToolCall {
            id: first.id,
            name: first.name,
            arguments: first.arguments,
            tokens,
        });
    }

    if let Some(content_value) = &span.response_content {
        let blocks: Vec<ContentBlock> = serde_json::from_value(content_value.clone())
            .map_err(|source| CurateError::ContentDecode { step, source })?;
        let text = collect_text(step, &blocks)?;
        let finish_reason = finish_reason_from_span(span);
        return Ok(ProviderScriptEvent::ChatResponse {
            content: text,
            tokens,
            finish_reason,
        });
    }

    Err(CurateError::MissingContent { step })
}

fn build_tokens(span: &GenAISpan) -> TokenUsage {
    TokenUsage {
        prompt_tokens: span.input_tokens,
        completion_tokens: span.output_tokens,
        total_tokens: span.total_tokens,
        cache_read_tokens: span.cache_read_input_tokens,
        cache_creation_tokens: span.cache_creation_input_tokens,
        thinking_tokens: span.thinking_tokens,
    }
}

/// Concatenate every `ContentBlock::Text` payload, in order. Non-text
/// blocks are rejected instead of dropped: `ProviderScriptEvent::ChatResponse`
/// is currently text-only, and silently curating a lossy script would
/// make the resulting fixture pass/fail against behaviour it never
/// actually captured.
fn collect_text(step: u32, blocks: &[ContentBlock]) -> Result<String, CurateError> {
    let mut out = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text, .. } => out.push_str(text),
            other => {
                return Err(CurateError::UnsupportedContentBlock {
                    step,
                    block_type: content_block_kind(other).to_string(),
                });
            }
        }
    }
    Ok(out)
}

fn content_block_kind(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text { .. } => "text",
        ContentBlock::Image { .. } => "image",
        ContentBlock::Document { .. } => "document",
        ContentBlock::Audio { .. } => "audio",
        ContentBlock::Video { .. } => "video",
        ContentBlock::ToolUse { .. } => "tool_use",
        ContentBlock::ToolResult { .. } => "tool_result",
        ContentBlock::Thinking { .. } => "thinking",
    }
}

fn finish_reason_from_span(span: &GenAISpan) -> StopReason {
    // GenAISpan stores OTel-aligned finish-reason strings (see
    // `stop_reason_to_finish_reason` in awaken-ext-observability hooks).
    // The mapping below mirrors it. Unknown strings fall through to
    // `EndTurn` — that matches the executor's own default for absent
    // finish_reason on legacy fixtures.
    match span.finish_reasons.first().map(String::as_str) {
        Some("end_turn") => StopReason::EndTurn,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("tool_use") => StopReason::ToolUse,
        Some("stop_sequence") => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_ext_observability::{SpanContext, ToolSpan};
    use serde_json::json;

    fn span(model: &str, step: u32) -> GenAISpan {
        GenAISpan {
            context: SpanContext::default(),
            step_index: Some(step),
            model: model.into(),
            provider: "p".into(),
            operation: "chat".into(),
            response_model: None,
            response_id: None,
            finish_reasons: vec!["end_turn".into()],
            error_type: None,
            error_class: None,
            thinking_tokens: None,
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: Vec::new(),
            duration_ms: 1,
            started_at_ms: 0,
            ended_at_ms: 0,
            response_content: Some(json!([{"type": "text", "text": "hello"}])),
            response_tool_calls: None,
            request_messages: None,
        }
    }

    fn tool_span(name: &str) -> ToolSpan {
        ToolSpan {
            context: SpanContext::default(),
            step_index: None,
            name: name.into(),
            operation: "execute_tool".into(),
            call_id: format!("call-{name}"),
            tool_type: "function".into(),
            call_arguments: None,
            call_result: None,
            error_type: None,
            duration_ms: 1,
            started_at_ms: 0,
            ended_at_ms: 0,
        }
    }

    #[test]
    fn first_user_text_recovered_from_request_messages() {
        // Inference captured `request_messages` = [user "ping"]. The
        // converter must surface "ping" as the recovered user_input so
        // the CLI / server can omit `--user-input`.
        let mut s = span("m", 0);
        s.request_messages = Some(json!([
            {"role": "user", "content": [{"type": "text", "text": "ping"}]}
        ]));
        let out = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap();
        assert_eq!(out.user_input.as_deref(), Some("ping"));
    }

    #[test]
    fn first_user_text_skips_system_and_assistant_messages() {
        // The user message isn't always first — the converter must scan
        // past system/assistant entries before reaching it.
        let mut s = span("m", 0);
        s.request_messages = Some(json!([
            {"role": "system", "content": [{"type": "text", "text": "be brief"}]},
            {"role": "user", "content": [{"type": "text", "text": "real prompt"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "..."}]},
        ]));
        let out = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap();
        assert_eq!(out.user_input.as_deref(), Some("real prompt"));
    }

    #[test]
    fn user_input_picks_latest_user_message_on_continuation_thread() {
        // Continuation thread: the user kicked off the thread weeks ago,
        // then sent another message today. The fixture should reflect
        // *what triggered this run*, not the original thread starter.
        let mut s = span("m", 0);
        s.request_messages = Some(json!([
            {"role": "user", "content": [{"type": "text", "text": "old prompt"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "old reply"}]},
            {"role": "user", "content": [{"type": "text", "text": "fresh prompt"}]},
        ]));
        let out = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap();
        assert_eq!(out.user_input.as_deref(), Some("fresh prompt"));
    }

    #[test]
    fn first_user_text_concatenates_multiple_text_blocks() {
        let mut s = span("m", 0);
        s.request_messages = Some(json!([
            {"role": "user", "content": [
                {"type": "text", "text": "part one "},
                {"type": "text", "text": "part two"},
            ]}
        ]));
        let out = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap();
        assert_eq!(out.user_input.as_deref(), Some("part one part two"));
    }

    #[test]
    fn user_input_is_none_when_request_messages_not_captured() {
        // Legacy trace (no ContentCapture) — operator must supply user_input.
        let s = span("m", 0);
        assert!(s.request_messages.is_none());
        let out = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap();
        assert!(out.user_input.is_none());
    }

    #[test]
    fn user_input_is_none_when_request_messages_has_no_user_role() {
        // A trace whose first inference somehow lacks the user message
        // (e.g. a system-prompt-only health-check run). Surface None
        // rather than guessing.
        let mut s = span("m", 0);
        s.request_messages = Some(json!([
            {"role": "system", "content": [{"type": "text", "text": "ping"}]}
        ]));
        let out = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap();
        assert!(out.user_input.is_none());
    }

    #[test]
    fn empty_events_is_no_inferences() {
        let err = trace_to_provider_script(&[]).unwrap_err();
        assert!(matches!(err, CurateError::NoInferences));
    }

    #[test]
    fn events_without_any_inference_is_no_inferences() {
        // Only tool spans; nothing for the scripted executor to model.
        let events = vec![MetricsEvent::Tool(tool_span("search"))];
        let err = trace_to_provider_script(&events).unwrap_err();
        assert!(matches!(err, CurateError::NoInferences));
    }

    #[test]
    fn single_chat_inference_round_trips_text_tokens_and_finish_reason() {
        let events = vec![MetricsEvent::Inference(span("claude-opus-4-7", 0))];
        let out = trace_to_provider_script(&events).unwrap();
        assert_eq!(out.source_model_id.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(out.provider_script.len(), 1);
        match &out.provider_script[0] {
            ProviderScriptEvent::ChatResponse {
                content,
                tokens,
                finish_reason,
            } => {
                assert_eq!(content, "hello");
                assert_eq!(tokens.prompt_tokens, Some(10));
                assert_eq!(tokens.completion_tokens, Some(5));
                assert_eq!(tokens.total_tokens, Some(15));
                assert_eq!(*finish_reason, StopReason::EndTurn);
            }
            other => panic!("expected ChatResponse, got {other:?}"),
        }
    }

    #[test]
    fn source_model_id_pins_first_inference_model() {
        // Subsequent spans don't override the source — the *first* model
        // is what the model-guard later matches against.
        let mut a = span("model-a", 0);
        let mut b = span("model-b", 1);
        a.response_content = Some(json!([{"type": "text", "text": "first"}]));
        b.response_content = Some(json!([{"type": "text", "text": "second"}]));
        let events = vec![MetricsEvent::Inference(a), MetricsEvent::Inference(b)];
        let out = trace_to_provider_script(&events).unwrap();
        assert_eq!(out.source_model_id.as_deref(), Some("model-a"));
        assert_eq!(out.provider_script.len(), 2);
    }

    #[test]
    fn empty_model_skipped_until_first_non_empty() {
        // A trace whose first inference span lost its model name still
        // surfaces the next span's model. Catches the seam where a
        // misconfigured provider records an empty `model` string.
        let mut a = span("", 0);
        a.response_content = Some(json!([{"type": "text", "text": "blank"}]));
        let b = span("model-b", 1);
        let events = vec![MetricsEvent::Inference(a), MetricsEvent::Inference(b)];
        let out = trace_to_provider_script(&events).unwrap();
        assert_eq!(out.source_model_id.as_deref(), Some("model-b"));
    }

    #[test]
    fn tool_spans_between_inferences_are_skipped() {
        // Tool execution is runtime-side; the scripted executor only
        // models the upstream LLM. Tool spans must NOT inflate the
        // returned script.
        let events = vec![
            MetricsEvent::Inference(span("m", 0)),
            MetricsEvent::Tool(tool_span("search")),
            MetricsEvent::Tool(tool_span("write")),
            MetricsEvent::Inference(span("m", 1)),
        ];
        let out = trace_to_provider_script(&events).unwrap();
        assert_eq!(out.provider_script.len(), 2);
    }

    #[test]
    fn tool_call_response_becomes_tool_call_event() {
        let mut s = span("m", 0);
        s.finish_reasons = vec!["tool_use".into()];
        s.response_content = None;
        s.response_tool_calls = Some(json!([
            {"id": "call-1", "name": "weather.get", "arguments": {"city": "Paris"}}
        ]));
        let events = vec![MetricsEvent::Inference(s)];
        let out = trace_to_provider_script(&events).unwrap();
        match &out.provider_script[0] {
            ProviderScriptEvent::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "weather.get");
                assert_eq!(arguments, &json!({"city": "Paris"}));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_with_content_is_an_error() {
        // A turn that recorded BOTH text and a tool call (e.g. an
        // assistant that explains then calls) is not faithfully
        // representable by ProviderScriptEvent::ToolCall, which carries
        // only the call. Refuse to drop the assistant text silently.
        let mut s = span("m", 0);
        s.response_content = Some(json!([{"type": "text", "text": "let me look that up"}]));
        s.response_tool_calls = Some(json!([
            {"id": "c1", "name": "search", "arguments": {}}
        ]));
        let err = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap_err();
        assert!(matches!(err, CurateError::MixedToolCallContent { step: 0 }));
    }

    #[test]
    fn empty_tool_calls_array_is_an_error() {
        // StopReason::ToolUse with zero calls is not representable as
        // ProviderScriptEvent::ToolCall — surface a real error so the
        // operator notices a malformed trace instead of silently
        // dropping the turn.
        let mut s = span("m", 0);
        s.response_content = None;
        s.response_tool_calls = Some(json!([]));
        let err = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap_err();
        assert!(matches!(err, CurateError::EmptyToolCalls { step: 0 }));
    }

    #[test]
    fn multiple_tool_calls_is_an_error() {
        let mut s = span("m", 0);
        s.finish_reasons = vec!["tool_use".into()];
        s.response_content = None;
        s.response_tool_calls = Some(json!([
            {"id": "call-1", "name": "search", "arguments": {"q": "a"}},
            {"id": "call-2", "name": "write", "arguments": {"text": "b"}}
        ]));
        let err = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap_err();
        assert!(matches!(
            err,
            CurateError::MultipleToolCalls { step: 0, count: 2 }
        ));
    }

    #[test]
    fn error_path_inference_becomes_error_event() {
        // After-inference hook sets `error_type` on the failure branch
        // and skips content capture. The converter must surface that as
        // a scripted Error event so replay reproduces the failure mode.
        let mut s = span("m", 0);
        s.error_type = Some("rate_limit".into());
        s.response_content = None;
        s.response_tool_calls = None;
        let out = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap();
        match &out.provider_script[0] {
            ProviderScriptEvent::Error { error_type, .. } => {
                assert_eq!(error_type, "rate_limit");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn inference_without_capture_or_error_is_missing_content() {
        // Span recorded without ContentCapture::Enabled and without a
        // failure — there's nothing to script. Don't invent a default,
        // tell the operator to enable capture.
        let mut s = span("m", 0);
        s.response_content = None;
        s.response_tool_calls = None;
        let err = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap_err();
        assert!(matches!(err, CurateError::MissingContent { step: 0 }));
    }

    #[test]
    fn malformed_response_content_surfaces_decode_error() {
        let mut s = span("m", 0);
        s.response_content = Some(json!("not an array of content blocks"));
        let err = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap_err();
        assert!(matches!(err, CurateError::ContentDecode { step: 0, .. }));
    }

    #[test]
    fn finish_reasons_mapping_covers_every_otel_string() {
        for (otel, expected) in [
            ("end_turn", StopReason::EndTurn),
            ("max_tokens", StopReason::MaxTokens),
            ("tool_use", StopReason::ToolUse),
            ("stop_sequence", StopReason::StopSequence),
        ] {
            let mut s = span("m", 0);
            s.finish_reasons = vec![otel.into()];
            let out = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap();
            match &out.provider_script[0] {
                ProviderScriptEvent::ChatResponse { finish_reason, .. } => {
                    assert_eq!(*finish_reason, expected, "otel string {otel}");
                }
                other => panic!("expected ChatResponse for {otel}, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_finish_reason_defaults_to_end_turn() {
        let mut s = span("m", 0);
        s.finish_reasons = vec!["something-new".into()];
        let out = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap();
        if let ProviderScriptEvent::ChatResponse { finish_reason, .. } = &out.provider_script[0] {
            assert_eq!(*finish_reason, StopReason::EndTurn);
        } else {
            panic!("expected ChatResponse");
        }
    }

    #[test]
    fn non_text_chat_content_is_an_error() {
        let mut s = span("m", 0);
        s.response_content = Some(json!([
            {"type": "text", "text": "part one "},
            {"type": "thinking", "thinking": "internal monologue", "signature": null},
            {"type": "text", "text": "part two"},
        ]));
        let err = trace_to_provider_script(&[MetricsEvent::Inference(s)]).unwrap_err();
        assert!(matches!(
            err,
            CurateError::UnsupportedContentBlock {
                step: 0,
                block_type
            } if block_type == "thinking"
        ));
    }
}
