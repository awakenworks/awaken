//! Scripted LLM executor that replays a `Vec<ProviderScriptEvent>`.
//!
//! Each call to `execute` pops the next event off the front of the script
//! and produces a [`StreamResult`] whose numbers (tokens, stop reason,
//! tool call arguments) come straight from the script — no heuristics.
//! When the runtime's observability plugin records spans for the returned
//! result, the spans carry the script's numbers verbatim, so replayed
//! evaluations are bit-for-bit reproducible against the same script.
//!
//! Used by `awaken-eval`'s `RuntimeReplayer` and by integration tests
//! that need a deterministic upstream.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::message::ToolCall;

/// One scripted upstream turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderScriptEvent {
    /// Plain text reply.
    ChatResponse {
        content: String,
        #[serde(default)]
        tokens: TokenUsage,
        #[serde(default = "default_finish_reason")]
        finish_reason: StopReason,
    },
    /// Tool-use turn. Becomes a `StreamResult` with `stop_reason = ToolUse`.
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
        #[serde(default)]
        tokens: TokenUsage,
    },
    /// Inference error path. The executor returns the mapped
    /// [`InferenceExecutionError`] instead of a `StreamResult`. `error_type`
    /// is matched case-insensitively against known variants; anything
    /// else falls through to `InferenceExecutionError::Provider`.
    Error {
        error_type: String,
        #[serde(default)]
        message: String,
    },
}

fn default_finish_reason() -> StopReason {
    StopReason::EndTurn
}

/// LLM executor that returns successive [`ProviderScriptEvent`]s.
pub struct ScriptedLlmExecutor {
    script: Mutex<VecDeque<ProviderScriptEvent>>,
    name: String,
    /// When set, every `execute` call must carry an `InferenceRequest`
    /// whose `upstream_model` matches this string. A mismatch returns
    /// `InvalidRequest` *without* consuming a scripted event — eval
    /// replay uses this as the fixture-vs-runtime model guard promised
    /// by `Fixture::source_model_id`.
    expected_upstream_model: Option<String>,
    /// Error type of the first scripted `Error` event that fired, captured
    /// before it gets wrapped in `AgentLoopError::InferenceFailed(String)`
    /// (which loses the structured variant). Read after the run by
    /// callers that need to assert on the kind of failure.
    first_error: Mutex<Option<(String, String)>>,
    /// Total `execute` calls that consumed (popped) a scripted event.
    /// Lets eval replay assert "the runtime called inference exactly N
    /// times" instead of inferring it from `remaining()` alone — which
    /// can't distinguish "no retry" from "retried but `first_error` was
    /// already captured".
    consumed_calls: AtomicUsize,
    /// `execute` calls that found the script empty and returned
    /// `InvalidRequest("scripted executor exhausted: ...")`. Eval replay
    /// surfaces this as a `ReplayRuntimeFailure::ScriptExhausted` so a
    /// runtime over-call doesn't masquerade as the originally captured
    /// error.
    exhausted_calls: AtomicUsize,
    /// Total `execute` calls that fired a scripted `Error` event. Eval
    /// replay exposes this as `ReplayReport::inference_error_count` so
    /// failure-path fixtures don't appear as "0 inferences happened".
    error_calls: AtomicUsize,
}

impl ScriptedLlmExecutor {
    pub fn new<I>(events: I) -> Self
    where
        I: IntoIterator<Item = ProviderScriptEvent>,
    {
        Self {
            script: Mutex::new(events.into_iter().collect()),
            name: "scripted".to_string(),
            expected_upstream_model: None,
            first_error: Mutex::new(None),
            consumed_calls: AtomicUsize::new(0),
            exhausted_calls: AtomicUsize::new(0),
            error_calls: AtomicUsize::new(0),
        }
    }

    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Require every incoming `InferenceRequest.upstream_model` to match
    /// `model`. Replay rejects mismatches as `InvalidRequest` and does
    /// not consume a scripted event.
    #[must_use]
    pub fn with_expected_upstream_model(mut self, model: impl Into<String>) -> Self {
        self.expected_upstream_model = Some(model.into());
        self
    }

    /// Number of events still queued. Test-only convenience.
    pub fn remaining(&self) -> usize {
        self.script.lock().expect("script mutex poisoned").len()
    }

    /// Number of `execute` calls that popped a scripted event.
    pub fn consumed_calls(&self) -> usize {
        self.consumed_calls.load(Ordering::Relaxed)
    }

    /// Number of `execute` calls that found the script empty and
    /// returned `InvalidRequest`. Non-zero means the runtime called
    /// inference more times than the fixture provided events for —
    /// retry firing, an unexpected tool round, or a fixture that under-
    /// specifies the script.
    pub fn exhausted_calls(&self) -> usize {
        self.exhausted_calls.load(Ordering::Relaxed)
    }

    /// Number of `execute` calls that fired a scripted `Error` event.
    pub fn error_calls(&self) -> usize {
        self.error_calls.load(Ordering::Relaxed)
    }

    /// `(error_type, message)` of the first scripted `Error` event that
    /// fired during this executor's lifetime, or `None` if no error event
    /// was issued. Eval replay reads this to surface the kind of failure
    /// even though `AgentLoopError::InferenceFailed(String)` flattens the
    /// underlying variant.
    pub fn first_error(&self) -> Option<(String, String)> {
        self.first_error
            .lock()
            .expect("first_error mutex poisoned")
            .clone()
    }
}

#[async_trait]
impl LlmExecutor for ScriptedLlmExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        if let Some(expected) = self.expected_upstream_model.as_deref()
            && request.upstream_model != expected
        {
            return Err(InferenceExecutionError::InvalidRequest(format!(
                "scripted executor: upstream_model mismatch (expected {:?}, got {:?})",
                expected, request.upstream_model
            )));
        }
        let popped = self
            .script
            .lock()
            .expect("script mutex poisoned")
            .pop_front();
        let event = match popped {
            Some(e) => e,
            None => {
                // The runtime asked for inference but the fixture's
                // script is empty. Surface this distinctly so retries /
                // extra rounds / over-calls don't get swallowed into
                // `first_error`'s already-captured value.
                self.exhausted_calls.fetch_add(1, Ordering::Relaxed);
                return Err(InferenceExecutionError::InvalidRequest(
                    "scripted executor exhausted: no remaining provider_script events".to_string(),
                ));
            }
        };
        self.consumed_calls.fetch_add(1, Ordering::Relaxed);
        // Capture the *fixture-author-supplied* error_type string before
        // we hand the event off to `result_for`. After that point the
        // variant is opaque to callers (the runtime wraps it in
        // `AgentLoopError::InferenceFailed(String)`, losing structure).
        if let ProviderScriptEvent::Error {
            error_type,
            message,
        } = &event
        {
            self.error_calls.fetch_add(1, Ordering::Relaxed);
            let mut slot = self.first_error.lock().expect("first_error mutex poisoned");
            if slot.is_none() {
                *slot = Some((error_type.clone(), message.clone()));
            }
        }
        result_for(event)
    }

    fn name(&self) -> &str {
        &self.name
    }
}

fn result_for(event: ProviderScriptEvent) -> Result<StreamResult, InferenceExecutionError> {
    match event {
        ProviderScriptEvent::ChatResponse {
            content,
            tokens,
            finish_reason,
            ..
        } => Ok(StreamResult {
            content: vec![ContentBlock::text(content)],
            tool_calls: vec![],
            usage: Some(tokens),
            stop_reason: Some(finish_reason),
            has_incomplete_tool_calls: false,
        }),
        ProviderScriptEvent::ToolCall {
            id,
            name,
            arguments,
            tokens,
        } => Ok(StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new(id, name, arguments)],
            usage: Some(tokens),
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        }),
        ProviderScriptEvent::Error {
            error_type,
            message,
        } => Err(inference_error_from(&error_type, message)),
    }
}

fn inference_error_from(error_type: &str, message: String) -> InferenceExecutionError {
    match error_type.to_ascii_lowercase().as_str() {
        "rate_limit" | "rate_limited" => InferenceExecutionError::rate_limited(message),
        "overloaded" => InferenceExecutionError::overloaded(message),
        "timeout" => InferenceExecutionError::Timeout(message),
        "context_overflow" => InferenceExecutionError::ContextOverflow(message),
        "invalid_request" => InferenceExecutionError::InvalidRequest(message),
        "unauthorized" => InferenceExecutionError::Unauthorized(message),
        "model_not_found" => InferenceExecutionError::ModelNotFound(message),
        "content_filtered" => InferenceExecutionError::ContentFiltered(message),
        _ => InferenceExecutionError::Provider(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::message::Message;
    use serde_json::json;

    fn make_request() -> InferenceRequest {
        InferenceRequest {
            upstream_model: "scripted".into(),
            routing_key: None,
            messages: vec![Message::user("hello")],
            tools: vec![],
            system: vec![],
            overrides: None,
            enable_prompt_cache: false,
        }
    }

    #[tokio::test]
    async fn chat_response_event_round_trips_tokens_and_stop_reason() {
        let executor = ScriptedLlmExecutor::new([ProviderScriptEvent::ChatResponse {
            content: "hi".into(),
            tokens: TokenUsage {
                prompt_tokens: Some(7),
                completion_tokens: Some(3),
                total_tokens: Some(10),
                ..Default::default()
            },
            finish_reason: StopReason::EndTurn,
        }]);

        let result = executor.execute(make_request()).await.unwrap();

        assert_eq!(result.text(), "hi");
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        assert!(!result.needs_tools());
        let usage = result.usage.expect("usage present");
        assert_eq!(usage.prompt_tokens, Some(7));
        assert_eq!(usage.completion_tokens, Some(3));
        assert_eq!(usage.total_tokens, Some(10));
    }

    #[tokio::test]
    async fn tool_call_event_synthesises_tool_use_stop() {
        let executor = ScriptedLlmExecutor::new([ProviderScriptEvent::ToolCall {
            id: "call-1".into(),
            name: "weather.get".into(),
            arguments: json!({ "city": "Paris" }),
            tokens: TokenUsage {
                prompt_tokens: Some(12),
                completion_tokens: Some(4),
                total_tokens: Some(16),
                ..Default::default()
            },
        }]);

        let result = executor.execute(make_request()).await.unwrap();

        assert!(result.text().is_empty());
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
        assert!(result.needs_tools());
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "call-1");
        assert_eq!(result.tool_calls[0].name, "weather.get");
        assert_eq!(result.tool_calls[0].arguments, json!({ "city": "Paris" }));
    }

    #[tokio::test]
    async fn events_are_consumed_in_fifo_order() {
        let executor = ScriptedLlmExecutor::new([
            ProviderScriptEvent::ChatResponse {
                content: "first".into(),
                tokens: TokenUsage::default(),
                finish_reason: StopReason::EndTurn,
            },
            ProviderScriptEvent::ChatResponse {
                content: "second".into(),
                tokens: TokenUsage::default(),
                finish_reason: StopReason::EndTurn,
            },
        ]);

        assert_eq!(executor.remaining(), 2);
        assert_eq!(
            executor.execute(make_request()).await.unwrap().text(),
            "first"
        );
        assert_eq!(executor.remaining(), 1);
        assert_eq!(
            executor.execute(make_request()).await.unwrap().text(),
            "second"
        );
        assert_eq!(executor.remaining(), 0);
    }

    #[tokio::test]
    async fn exhausted_script_returns_invalid_request() {
        let executor = ScriptedLlmExecutor::new(std::iter::empty());

        let err = executor
            .execute(make_request())
            .await
            .expect_err("exhausted executor should error");
        assert!(matches!(err, InferenceExecutionError::InvalidRequest(_)));
        assert!(!err.is_retryable());
    }

    #[test]
    fn provider_script_event_serde_uses_kind_tag() {
        let event = ProviderScriptEvent::ChatResponse {
            content: "ok".into(),
            tokens: TokenUsage::default(),
            finish_reason: StopReason::EndTurn,
        };
        let s = serde_json::to_string(&event).unwrap();
        assert!(s.contains(r#""kind":"chat_response""#));
        let parsed: ProviderScriptEvent = serde_json::from_str(&s).unwrap();
        match parsed {
            ProviderScriptEvent::ChatResponse { content, .. } => assert_eq!(content, "ok"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tool_call_serde_round_trip() {
        let event = ProviderScriptEvent::ToolCall {
            id: "t".into(),
            name: "noop".into(),
            arguments: json!({}),
            tokens: TokenUsage::default(),
        };
        let s = serde_json::to_string(&event).unwrap();
        let parsed: ProviderScriptEvent = serde_json::from_str(&s).unwrap();
        assert!(matches!(parsed, ProviderScriptEvent::ToolCall { .. }));
    }

    #[test]
    fn missing_finish_reason_defaults_to_end_turn() {
        // Default applies when the field is omitted (legacy fixtures).
        let json = r#"{"kind":"chat_response","content":"hi"}"#;
        let parsed: ProviderScriptEvent = serde_json::from_str(json).unwrap();
        match parsed {
            ProviderScriptEvent::ChatResponse { finish_reason, .. } => {
                assert_eq!(finish_reason, StopReason::EndTurn)
            }
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn error_event_maps_known_error_types() {
        let cases = [
            ("rate_limit", "is_rate_limited"),
            ("overloaded", "is_overloaded"),
            ("timeout", "is_timeout"),
            ("context_overflow", "is_context_overflow"),
            ("invalid_request", "is_invalid_request"),
            ("unauthorized", "is_unauthorized"),
            ("model_not_found", "is_model_not_found"),
            ("content_filtered", "is_content_filtered"),
        ];
        for (kind, _label) in cases {
            let executor = ScriptedLlmExecutor::new([ProviderScriptEvent::Error {
                error_type: kind.into(),
                message: format!("oops {kind}"),
            }]);
            let err = executor.execute(make_request()).await.unwrap_err();
            // Variant-specific shape: just confirm display includes the message
            // and the variant matches what we mapped.
            let msg = format!("{err}");
            assert!(
                msg.contains(kind) || msg.contains("oops"),
                "kind {kind} -> {msg}"
            );
            match (kind, &err) {
                ("rate_limit", InferenceExecutionError::RateLimited { .. }) => {}
                ("overloaded", InferenceExecutionError::Overloaded { .. }) => {}
                ("timeout", InferenceExecutionError::Timeout(_)) => {}
                ("context_overflow", InferenceExecutionError::ContextOverflow(_)) => {}
                ("invalid_request", InferenceExecutionError::InvalidRequest(_)) => {}
                ("unauthorized", InferenceExecutionError::Unauthorized(_)) => {}
                ("model_not_found", InferenceExecutionError::ModelNotFound(_)) => {}
                ("content_filtered", InferenceExecutionError::ContentFiltered(_)) => {}
                _ => panic!("kind {kind} mapped to wrong variant: {err:?}"),
            }
        }
    }

    #[tokio::test]
    async fn error_event_unknown_type_falls_through_to_provider() {
        let executor = ScriptedLlmExecutor::new([ProviderScriptEvent::Error {
            error_type: "weird".into(),
            message: "??".into(),
        }]);
        let err = executor.execute(make_request()).await.unwrap_err();
        assert!(matches!(err, InferenceExecutionError::Provider(_)));
    }

    #[tokio::test]
    async fn first_error_captures_initial_error_event() {
        let executor = ScriptedLlmExecutor::new([
            ProviderScriptEvent::Error {
                error_type: "rate_limit".into(),
                message: "429".into(),
            },
            ProviderScriptEvent::Error {
                error_type: "unauthorized".into(),
                message: "401".into(),
            },
        ]);
        let _ = executor.execute(make_request()).await.unwrap_err();
        let _ = executor.execute(make_request()).await.unwrap_err();
        let (kind, msg) = executor.first_error().expect("first_error captured");
        assert_eq!(kind, "rate_limit");
        assert_eq!(msg, "429");
    }

    #[tokio::test]
    async fn first_error_none_when_only_success_events_run() {
        let executor = ScriptedLlmExecutor::new([ProviderScriptEvent::ChatResponse {
            content: "ok".into(),
            tokens: TokenUsage::default(),
            finish_reason: StopReason::EndTurn,
        }]);
        let _ = executor.execute(make_request()).await.unwrap();
        assert!(executor.first_error().is_none());
    }

    #[tokio::test]
    async fn consumed_and_exhausted_call_counters_track_independently() {
        let executor = ScriptedLlmExecutor::new([ProviderScriptEvent::ChatResponse {
            content: "ok".into(),
            tokens: TokenUsage::default(),
            finish_reason: StopReason::EndTurn,
        }]);
        // First call pops the event.
        let _ = executor.execute(make_request()).await.unwrap();
        assert_eq!(executor.consumed_calls(), 1);
        assert_eq!(executor.exhausted_calls(), 0);
        assert_eq!(executor.error_calls(), 0);

        // Second + third calls find the script empty.
        let _ = executor.execute(make_request()).await.unwrap_err();
        let _ = executor.execute(make_request()).await.unwrap_err();
        assert_eq!(executor.consumed_calls(), 1);
        assert_eq!(executor.exhausted_calls(), 2);
    }

    #[tokio::test]
    async fn error_call_counter_only_counts_scripted_error_events() {
        let executor = ScriptedLlmExecutor::new([
            ProviderScriptEvent::Error {
                error_type: "rate_limit".into(),
                message: "429".into(),
            },
            ProviderScriptEvent::ChatResponse {
                content: "ok".into(),
                tokens: TokenUsage::default(),
                finish_reason: StopReason::EndTurn,
            },
        ]);
        let _ = executor.execute(make_request()).await.unwrap_err();
        let _ = executor.execute(make_request()).await.unwrap();
        assert_eq!(executor.error_calls(), 1);
        assert_eq!(executor.consumed_calls(), 2);
        assert_eq!(executor.exhausted_calls(), 0);
    }

    #[tokio::test]
    async fn expected_upstream_model_mismatch_does_not_consume_event() {
        let executor = ScriptedLlmExecutor::new([ProviderScriptEvent::ChatResponse {
            content: "ok".into(),
            tokens: TokenUsage::default(),
            finish_reason: StopReason::EndTurn,
        }])
        .with_expected_upstream_model("claude-opus-4-7");

        let mut wrong = make_request();
        wrong.upstream_model = "gpt-4o".into();
        let err = executor.execute(wrong).await.unwrap_err();
        assert!(matches!(err, InferenceExecutionError::InvalidRequest(_)));
        assert_eq!(
            executor.remaining(),
            1,
            "mismatched call must not pop event"
        );

        let mut right = make_request();
        right.upstream_model = "claude-opus-4-7".into();
        let ok = executor.execute(right).await.unwrap();
        assert_eq!(ok.text(), "ok");
        assert_eq!(executor.remaining(), 0);
    }
}
