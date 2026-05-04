//! LLM executor trait and tool execution strategy.

use std::time::Duration;

use super::content::ContentBlock;
use super::inference::{InferenceOverride, StreamResult};
use super::message::{Message, ToolCall};
use super::tool::ToolDescriptor;
use async_trait::async_trait;
use thiserror::Error;

/// A provider-neutral LLM inference request.
#[derive(Debug, Clone)]
pub struct InferenceRequest {
    /// Effective upstream model name sent to the resolved provider executor.
    pub upstream_model: String,
    /// Messages to send.
    pub messages: Vec<Message>,
    /// Available tools.
    pub tools: Vec<ToolDescriptor>,
    /// System prompt content blocks. Empty means no system prompt.
    pub system: Vec<ContentBlock>,
    /// Per-inference overrides that remain after runtime routing is applied
    /// (temperature, max_tokens, fallback upstream models, etc).
    pub overrides: Option<InferenceOverride>,
    /// Whether to apply prompt cache hints (e.g. `CacheControl::Ephemeral`) to system messages.
    pub enable_prompt_cache: bool,
}

/// Cause of a mid-stream interruption.
#[derive(Debug, Clone)]
pub enum InterruptCause {
    /// Underlying socket reset (TCP RST, ECONNRESET) while receiving events.
    ConnectionReset,
    /// No delta received within the configured idle window.
    IdleStall,
    /// HTTP/2 GOAWAY or equivalent server-initiated disconnect.
    GoAway,
    /// Provider returned a 5xx status after headers had been sent.
    Provider5xxMidStream(u16),
    /// Synthetic cause used when a stream is being resumed from a
    /// persisted checkpoint (no real interruption happened in this
    /// process — the previous process crashed or restarted).
    ResumedFromCheckpoint,
}

impl std::fmt::Display for InterruptCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectionReset => f.write_str("connection reset"),
            Self::IdleStall => f.write_str("idle stall"),
            Self::GoAway => f.write_str("goaway"),
            Self::Provider5xxMidStream(s) => write!(f, "provider {s} mid-stream"),
            Self::ResumedFromCheckpoint => f.write_str("resumed from checkpoint"),
        }
    }
}

/// A tool_use block observed mid-stream whose argument JSON did not close.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InFlightTool {
    pub id: String,
    pub name: String,
    /// Raw accumulated argument JSON fragment (unparseable as-is).
    pub partial_args: String,
}

/// Snapshot of everything a `StreamCollector` had accumulated at the moment
/// the stream was interrupted. Used by the loop runner to pick a
/// [`RecoveryPlan`](crate::contract::executor::RecoveryPlan).
#[derive(Debug, Clone)]
pub struct InterruptSnapshot {
    /// Assistant text accumulated before the interruption. `None` if no text
    /// was received.
    pub text: Option<String>,
    /// Tool calls whose argument JSON parsed successfully before interruption.
    pub completed_tool_calls: Vec<ToolCall>,
    /// The tool_use block (if any) that was open but not yet closed.
    pub in_flight_tool: Option<InFlightTool>,
    /// Total bytes of events processed (telemetry).
    pub bytes_received: usize,
}

/// Chosen recovery path for a mid-stream interruption. Computed from
/// [`InterruptSnapshot::plan`].
#[derive(Debug, Clone)]
pub enum RecoveryPlan {
    /// Only text accumulated. Retry the whole request with the accumulated
    /// text injected as an assistant prefix followed by a continuation
    /// prompt.
    ContinueText { assistant_prefix: String },
    /// At least one tool_use arrived intact. Synthesize a
    /// `StopReason::ToolUse` terminal state so the loop runner executes the
    /// completed tools. Any in-flight tool is surfaced as a hint for the
    /// next user message.
    SynthesizeToolUse {
        completed: Vec<ToolCall>,
        cancelled_tool_hint: Option<InFlightTool>,
    },
    /// There was text plus a single unclosed tool_use. Truncate before the
    /// tool, emit a cancel event for consumers, then continue with the text
    /// prefix.
    TruncateBeforeTool {
        assistant_prefix: String,
        cancelled_tool_id: String,
        cancelled_tool_name: String,
    },
    /// Nothing salvageable: retry the entire request fresh.
    WholeRestart,
}

impl InterruptSnapshot {
    /// Build an `InterruptSnapshot` from a stream of `(id, name, args_json)`
    /// triples in declaration order, plus the accumulated text.
    ///
    /// Tools whose `name` is empty or whose `args_json` does not parse as
    /// JSON become the `in_flight_tool` (last-write-wins if multiple);
    /// the rest land in `completed_tool_calls`. This is the single source
    /// of truth for partials → snapshot translation; the multiple
    /// stream-collector implementations across the runtime delegate to
    /// it instead of reimplementing.
    pub fn from_partials<I>(text: Option<String>, partials: I, bytes_received: usize) -> Self
    where
        I: IntoIterator<Item = (String, String, String)>,
    {
        let mut completed: Vec<ToolCall> = Vec::new();
        let mut in_flight: Option<InFlightTool> = None;

        for (id, name, args_json) in partials {
            if name.is_empty() {
                in_flight = Some(InFlightTool {
                    id,
                    name: String::new(),
                    partial_args: args_json,
                });
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(&args_json) {
                Ok(arguments) if !arguments.is_null() || args_json.is_empty() => {
                    completed.push(ToolCall::new(id, name, arguments));
                }
                _ => {
                    in_flight = Some(InFlightTool {
                        id,
                        name,
                        partial_args: args_json,
                    });
                }
            }
        }

        Self {
            text,
            completed_tool_calls: completed,
            in_flight_tool: in_flight,
            bytes_received,
        }
    }

    /// Decide which recovery plan applies to this snapshot.
    pub fn plan(&self) -> RecoveryPlan {
        let text = self.text.as_deref().unwrap_or("");
        let has_text = !text.is_empty();
        let has_completed = !self.completed_tool_calls.is_empty();

        // R2: any completed tool → synthesize ToolUse regardless of text/in-flight.
        if has_completed {
            return RecoveryPlan::SynthesizeToolUse {
                completed: self.completed_tool_calls.clone(),
                cancelled_tool_hint: self.in_flight_tool.clone(),
            };
        }

        // R3: text with an in-flight tool → truncate to the text prefix.
        if has_text {
            if let Some(p) = &self.in_flight_tool {
                return RecoveryPlan::TruncateBeforeTool {
                    assistant_prefix: text.to_string(),
                    cancelled_tool_id: p.id.clone(),
                    cancelled_tool_name: p.name.clone(),
                };
            }
            // R1: text only.
            return RecoveryPlan::ContinueText {
                assistant_prefix: text.to_string(),
            };
        }

        // R4: nothing usable (no text and no completed tools).
        RecoveryPlan::WholeRestart
    }
}

/// Errors from LLM inference.
///
/// Variants split into three recoverability classes:
/// - **Transient** (retryable, count toward circuit breaker): `RateLimited`,
///   `Overloaded`, `Timeout`, `Provider`, `StreamInterrupted`.
/// - **Permanent** (not retryable, do NOT count toward circuit breaker):
///   `ContextOverflow`, `InvalidRequest`, `Unauthorized`, `ModelNotFound`,
///   `ContentFiltered`.
/// - **Fail-fast**: `AllModelsUnavailable`, `Cancelled`.
///
/// Use [`InferenceExecutionError::is_retryable`] and
/// [`InferenceExecutionError::counts_toward_circuit_breaker`] for policy
/// decisions instead of pattern-matching variants directly where possible.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum InferenceExecutionError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("rate limited: {message}")]
    RateLimited {
        message: String,
        /// Duration from the provider's `Retry-After` header, if any.
        retry_after: Option<Duration>,
    },
    #[error("provider overloaded: {message}")]
    Overloaded {
        message: String,
        retry_after: Option<Duration>,
    },
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("stream interrupted ({cause})")]
    StreamInterrupted {
        cause: InterruptCause,
        snapshot: Box<InterruptSnapshot>,
    },
    #[error("context overflow: {0}")]
    ContextOverflow(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("model not found: {0}")]
    ModelNotFound(String),
    #[error("content filtered: {0}")]
    ContentFiltered(String),
    #[error("all models unavailable (circuit breakers open)")]
    AllModelsUnavailable,
    #[error("cancelled")]
    Cancelled,
}

impl InferenceExecutionError {
    /// Short constructor for a rate-limit error with no `Retry-After`.
    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::RateLimited {
            message: message.into(),
            retry_after: None,
        }
    }

    /// Short constructor for an overloaded error with no `Retry-After`.
    pub fn overloaded(message: impl Into<String>) -> Self {
        Self::Overloaded {
            message: message.into(),
            retry_after: None,
        }
    }

    /// Whether the retry subsystem should try this request again.
    ///
    /// Transient errors return `true`; permanent and fail-fast errors
    /// (including `Cancelled`) return `false`.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Provider(_)
                | Self::RateLimited { .. }
                | Self::Overloaded { .. }
                | Self::Timeout(_)
                | Self::StreamInterrupted { .. }
        )
    }

    /// Whether this failure should increment the per-model circuit-breaker
    /// failure counter. Permanent errors (bad auth, bad schema, context
    /// overflow) must not trip the breaker — they would have failed with the
    /// same error on any model.
    pub fn counts_toward_circuit_breaker(&self) -> bool {
        self.is_retryable()
    }

    /// If this error carries a `Retry-After` hint from the provider, return it.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. } | Self::Overloaded { retry_after, .. } => {
                *retry_after
            }
            _ => None,
        }
    }
}

/// A token-level streaming event from the LLM.
#[derive(Debug, Clone)]
pub enum LlmStreamEvent {
    /// Incremental text content.
    TextDelta(String),
    /// Incremental reasoning/thinking content.
    ReasoningDelta(String),
    /// A tool use block started.
    ToolCallStart { id: String, name: String },
    /// Incremental tool call argument JSON.
    ToolCallDelta { id: String, args_delta: String },
    /// A content block finished.
    ContentBlockStop,
    /// Token usage data (typically sent once at the end).
    Usage(super::inference::TokenUsage),
    /// Stop reason (end of stream).
    Stop(super::inference::StopReason),
}

/// A boxed stream of `LlmStreamEvent`s.
///
/// Implementors wrap their provider-specific streaming response into this type.
/// The loop runner consumes events, emits deltas via `EventSink`, and collects
/// the final `StreamResult`.
pub type InferenceStream = std::pin::Pin<
    Box<dyn futures::Stream<Item = Result<LlmStreamEvent, InferenceExecutionError>> + Send>,
>;

/// Abstraction over LLM inference backends.
///
/// Providers implement `execute` (collected) and optionally `execute_stream` (streaming).
/// The loop runner prefers `execute_stream` when available.
#[async_trait]
pub trait LlmExecutor: Send + Sync {
    /// Execute a chat completion and return the collected result.
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError>;

    /// Execute a chat completion as a token stream.
    ///
    /// Default implementation calls `execute()` and wraps the result as a single-event stream.
    /// Override to provide true token-level streaming from the LLM provider.
    fn execute_stream(
        &self,
        request: InferenceRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<InferenceStream, InferenceExecutionError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let result = self.execute(request).await?;
            let events = collected_to_stream_events(result);
            Ok(Box::pin(futures::stream::iter(events)) as InferenceStream)
        })
    }

    /// Provider name for logging/debugging.
    fn name(&self) -> &str;
}

/// Convert a collected `StreamResult` into a sequence of `LlmStreamEvent`s.
pub fn collected_to_stream_events(
    result: StreamResult,
) -> Vec<Result<LlmStreamEvent, InferenceExecutionError>> {
    use super::content::ContentBlock;
    let mut events = Vec::new();

    // Emit text/thinking deltas from content blocks
    for block in &result.content {
        match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                events.push(Ok(LlmStreamEvent::TextDelta(text.clone())));
            }
            ContentBlock::Thinking { thinking } if !thinking.is_empty() => {
                events.push(Ok(LlmStreamEvent::ReasoningDelta(thinking.clone())));
            }
            _ => {}
        }
    }

    // Emit tool calls
    for call in &result.tool_calls {
        events.push(Ok(LlmStreamEvent::ToolCallStart {
            id: call.id.clone(),
            name: call.name.clone(),
        }));
        let args = serde_json::to_string(&call.arguments).unwrap_or_default();
        if !args.is_empty() {
            events.push(Ok(LlmStreamEvent::ToolCallDelta {
                id: call.id.clone(),
                args_delta: args,
            }));
        }
    }

    // Emit usage
    if let Some(usage) = result.usage {
        events.push(Ok(LlmStreamEvent::Usage(usage)));
    }

    // Emit stop reason
    if let Some(stop) = result.stop_reason {
        events.push(Ok(LlmStreamEvent::Stop(stop)));
    }

    events
}

/// Tool execution strategy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolExecutionMode {
    /// Execute tool calls one at a time.
    #[default]
    Sequential,
    /// Execute all tool calls concurrently, batch approval gate.
    ParallelBatchApproval,
    /// Execute all tool calls concurrently, streaming results.
    ParallelStreaming,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::inference::{StopReason, TokenUsage};
    use crate::contract::message::ToolCall;
    use crate::contract::tool::ToolDescriptor;
    use serde_json::json;

    /// A mock LLM executor for testing.
    struct MockLlm {
        response_text: String,
        tool_calls: Vec<ToolCall>,
    }

    #[async_trait]
    impl LlmExecutor for MockLlm {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: if self.response_text.is_empty() {
                    vec![]
                } else {
                    vec![ContentBlock::text(self.response_text.clone())]
                },
                tool_calls: self.tool_calls.clone(),
                usage: Some(TokenUsage {
                    prompt_tokens: Some(100),
                    completion_tokens: Some(50),
                    total_tokens: Some(150),
                    ..Default::default()
                }),
                stop_reason: if self.tool_calls.is_empty() {
                    Some(StopReason::EndTurn)
                } else {
                    Some(StopReason::ToolUse)
                },
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    #[tokio::test]
    async fn mock_llm_returns_text() {
        let llm = MockLlm {
            response_text: "Hello!".into(),
            tool_calls: vec![],
        };
        let request = InferenceRequest {
            upstream_model: "test-model".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            system: vec![],
            overrides: None,
            enable_prompt_cache: false,
        };
        let result = llm.execute(request).await.unwrap();
        assert_eq!(result.text(), "Hello!");
        assert!(!result.needs_tools());
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
    }

    #[tokio::test]
    async fn mock_llm_returns_tool_calls() {
        let llm = MockLlm {
            response_text: String::new(),
            tool_calls: vec![ToolCall::new("c1", "search", json!({"q": "rust"}))],
        };
        let request = InferenceRequest {
            upstream_model: "test-model".into(),
            messages: vec![Message::user("search for rust")],
            tools: vec![ToolDescriptor::new("search", "search", "Web search")],
            system: vec![ContentBlock::text("You are helpful.")],
            overrides: None,
            enable_prompt_cache: false,
        };
        let result = llm.execute(request).await.unwrap();
        assert!(result.needs_tools());
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "search");
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
    }

    #[tokio::test]
    async fn mock_llm_with_overrides() {
        let llm = MockLlm {
            response_text: "ok".into(),
            tool_calls: vec![],
        };
        let request = InferenceRequest {
            upstream_model: "base-model".into(),
            messages: vec![],
            tools: vec![],
            system: vec![],
            overrides: Some(InferenceOverride {
                temperature: Some(0.7),
                ..Default::default()
            }),
            enable_prompt_cache: false,
        };
        let result = llm.execute(request).await.unwrap();
        assert_eq!(result.text(), "ok");
    }

    #[test]
    fn llm_executor_name_is_exposed() {
        let llm = MockLlm {
            response_text: String::new(),
            tool_calls: vec![],
        };

        assert_eq!(llm.name(), "mock");
    }

    #[test]
    fn tool_execution_mode_default_is_sequential() {
        assert_eq!(ToolExecutionMode::default(), ToolExecutionMode::Sequential);
    }

    #[test]
    fn inference_execution_error_display_strings_are_stable() {
        assert_eq!(
            InferenceExecutionError::Provider("provider failed".into()).to_string(),
            "provider error: provider failed"
        );
        assert_eq!(
            InferenceExecutionError::rate_limited("too many requests").to_string(),
            "rate limited: too many requests"
        );
        assert_eq!(
            InferenceExecutionError::overloaded("server overloaded").to_string(),
            "provider overloaded: server overloaded"
        );
        assert_eq!(
            InferenceExecutionError::Timeout("slow backend".into()).to_string(),
            "timeout: slow backend"
        );
        assert_eq!(
            InferenceExecutionError::ContextOverflow("prompt too long".into()).to_string(),
            "context overflow: prompt too long"
        );
        assert_eq!(
            InferenceExecutionError::InvalidRequest("bad schema".into()).to_string(),
            "invalid request: bad schema"
        );
        assert_eq!(
            InferenceExecutionError::Unauthorized("bad key".into()).to_string(),
            "unauthorized: bad key"
        );
        assert_eq!(
            InferenceExecutionError::ModelNotFound("no such model".into()).to_string(),
            "model not found: no such model"
        );
        assert_eq!(
            InferenceExecutionError::AllModelsUnavailable.to_string(),
            "all models unavailable (circuit breakers open)"
        );
        assert_eq!(InferenceExecutionError::Cancelled.to_string(), "cancelled");

        let stream_err = InferenceExecutionError::StreamInterrupted {
            cause: InterruptCause::ConnectionReset,
            snapshot: Box::new(InterruptSnapshot {
                text: None,
                completed_tool_calls: vec![],
                in_flight_tool: None,
                bytes_received: 0,
            }),
        };
        assert_eq!(
            stream_err.to_string(),
            "stream interrupted (connection reset)"
        );
    }

    #[test]
    fn is_retryable_partitions_variants() {
        use InferenceExecutionError::*;
        let partial_snapshot = || {
            Box::new(InterruptSnapshot {
                text: None,
                completed_tool_calls: vec![],
                in_flight_tool: None,
                bytes_received: 0,
            })
        };

        // Retryable
        assert!(Provider("x".into()).is_retryable());
        assert!(InferenceExecutionError::rate_limited("x").is_retryable());
        assert!(InferenceExecutionError::overloaded("x").is_retryable());
        assert!(Timeout("x".into()).is_retryable());
        assert!(
            StreamInterrupted {
                cause: InterruptCause::ConnectionReset,
                snapshot: partial_snapshot(),
            }
            .is_retryable()
        );

        // Permanent
        assert!(!ContextOverflow("x".into()).is_retryable());
        assert!(!InvalidRequest("x".into()).is_retryable());
        assert!(!Unauthorized("x".into()).is_retryable());
        assert!(!ModelNotFound("x".into()).is_retryable());
        assert!(!ContentFiltered("x".into()).is_retryable());

        // Fail-fast / lifecycle
        assert!(!AllModelsUnavailable.is_retryable());
        assert!(!Cancelled.is_retryable());
    }

    #[test]
    fn retry_after_is_only_exposed_for_rate_limit_variants() {
        use std::time::Duration;

        let rl = InferenceExecutionError::RateLimited {
            message: "429".into(),
            retry_after: Some(Duration::from_secs(5)),
        };
        assert_eq!(rl.retry_after(), Some(Duration::from_secs(5)));

        let ov = InferenceExecutionError::Overloaded {
            message: "529".into(),
            retry_after: Some(Duration::from_secs(10)),
        };
        assert_eq!(ov.retry_after(), Some(Duration::from_secs(10)));

        assert_eq!(
            InferenceExecutionError::Timeout("slow".into()).retry_after(),
            None
        );
    }

    #[test]
    fn plan_returns_continue_text_when_only_text_present() {
        let snap = InterruptSnapshot {
            text: Some("hello".into()),
            completed_tool_calls: vec![],
            in_flight_tool: None,
            bytes_received: 5,
        };
        match snap.plan() {
            RecoveryPlan::ContinueText { assistant_prefix } => {
                assert_eq!(assistant_prefix, "hello");
            }
            other => panic!("expected ContinueText, got {other:?}"),
        }
    }

    #[test]
    fn plan_returns_synthesize_tool_use_when_completed_tool_present() {
        use serde_json::json;
        let snap = InterruptSnapshot {
            text: Some("I'll search.".into()),
            completed_tool_calls: vec![ToolCall::new("c1", "search", json!({"q": "rust"}))],
            in_flight_tool: Some(InFlightTool {
                id: "c2".into(),
                name: "fetch".into(),
                partial_args: r#"{"url":"#.into(),
            }),
            bytes_received: 64,
        };
        match snap.plan() {
            RecoveryPlan::SynthesizeToolUse {
                completed,
                cancelled_tool_hint,
            } => {
                assert_eq!(completed.len(), 1);
                assert_eq!(completed[0].name, "search");
                let hint = cancelled_tool_hint.expect("in-flight tool becomes hint");
                assert_eq!(hint.name, "fetch");
            }
            other => panic!("expected SynthesizeToolUse, got {other:?}"),
        }
    }

    #[test]
    fn plan_returns_truncate_before_tool_when_text_and_in_flight_only() {
        let snap = InterruptSnapshot {
            text: Some("let me think".into()),
            completed_tool_calls: vec![],
            in_flight_tool: Some(InFlightTool {
                id: "c1".into(),
                name: "calc".into(),
                partial_args: r#"{"expr":"#.into(),
            }),
            bytes_received: 24,
        };
        match snap.plan() {
            RecoveryPlan::TruncateBeforeTool {
                assistant_prefix,
                cancelled_tool_id,
                cancelled_tool_name,
            } => {
                assert_eq!(assistant_prefix, "let me think");
                assert_eq!(cancelled_tool_id, "c1");
                assert_eq!(cancelled_tool_name, "calc");
            }
            other => panic!("expected TruncateBeforeTool, got {other:?}"),
        }
    }

    #[test]
    fn plan_returns_whole_restart_when_nothing_salvageable() {
        let snap = InterruptSnapshot {
            text: None,
            completed_tool_calls: vec![],
            in_flight_tool: None,
            bytes_received: 0,
        };
        assert!(matches!(snap.plan(), RecoveryPlan::WholeRestart));

        // Also: only an in-flight tool, no text → whole restart.
        let snap2 = InterruptSnapshot {
            text: None,
            completed_tool_calls: vec![],
            in_flight_tool: Some(InFlightTool {
                id: "c1".into(),
                name: "x".into(),
                partial_args: "{".into(),
            }),
            bytes_received: 1,
        };
        assert!(matches!(snap2.plan(), RecoveryPlan::WholeRestart));
    }
}
