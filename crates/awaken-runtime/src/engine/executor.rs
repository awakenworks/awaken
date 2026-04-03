//! GenAI-backed LLM executor implementation.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use genai::Client;
use genai::chat::ReasoningEffort as GenaiReasoningEffort;
use genai::chat::{ChatOptions, ChatStreamEvent};
use reqwest::StatusCode;

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, InferenceStream, LlmExecutor, LlmStreamEvent,
};
use awaken_contract::contract::inference::{
    ReasoningEffort as ContractReasoningEffort, StopReason, StreamResult,
};

use super::convert::{build_chat_request, from_genai_tool_call, map_stop_reason, map_usage};
use super::streaming::{StreamCollector, StreamOutput};

/// Default timeout for LLM inference calls (120 seconds).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Map awaken-contract reasoning effort to genai reasoning effort.
fn map_reasoning_effort(effort: &ContractReasoningEffort) -> GenaiReasoningEffort {
    match effort {
        ContractReasoningEffort::None => GenaiReasoningEffort::None,
        ContractReasoningEffort::Low => GenaiReasoningEffort::Low,
        ContractReasoningEffort::Medium => GenaiReasoningEffort::Medium,
        ContractReasoningEffort::High => GenaiReasoningEffort::High,
        ContractReasoningEffort::Max => GenaiReasoningEffort::Max,
        ContractReasoningEffort::Budget(n) => GenaiReasoningEffort::Budget(*n),
    }
}

fn stream_output_to_llm_event(output: StreamOutput) -> Option<LlmStreamEvent> {
    match output {
        StreamOutput::TextDelta(delta) => Some(LlmStreamEvent::TextDelta(delta)),
        StreamOutput::ReasoningDelta(delta) => Some(LlmStreamEvent::ReasoningDelta(delta)),
        StreamOutput::ToolCallStart { id, name } => {
            Some(LlmStreamEvent::ToolCallStart { id, name })
        }
        StreamOutput::ToolCallDelta { id, args_delta } => {
            Some(LlmStreamEvent::ToolCallDelta { id, args_delta })
        }
        StreamOutput::None => None,
    }
}

/// LLM executor backed by the `genai` crate.
///
/// Supports all providers that genai supports: OpenAI, Anthropic, Gemini, Ollama, etc.
/// Configured via environment variables (OPENAI_API_KEY, ANTHROPIC_API_KEY, etc.)
/// or via `genai::ClientConfig`.
pub struct GenaiExecutor {
    client: Client,
    default_options: Option<ChatOptions>,
    default_timeout: Duration,
}

impl GenaiExecutor {
    /// Create a new executor with default configuration.
    ///
    /// Provider selection is based on model name prefix (e.g., "gpt-" → OpenAI,
    /// "claude-" → Anthropic). API keys are read from environment variables.
    pub fn new() -> Self {
        Self {
            client: Client::default(),
            default_options: None,
            default_timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Create with a custom genai `Client`.
    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            default_options: None,
            default_timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Set default chat options (temperature, max_tokens, etc.).
    #[must_use]
    pub fn with_options(mut self, options: ChatOptions) -> Self {
        self.default_options = Some(options);
        self
    }

    /// Set the default timeout for inference calls.
    #[must_use]
    pub fn with_timeout(mut self, duration: Duration) -> Self {
        self.default_timeout = duration;
        self
    }

    fn build_options(&self, request: &InferenceRequest) -> ChatOptions {
        let mut opts = self
            .default_options
            .clone()
            .unwrap_or_default()
            .with_capture_usage(true)
            .with_capture_content(true)
            .with_capture_tool_calls(true);

        if let Some(ref ovr) = request.overrides {
            if let Some(temp) = ovr.temperature {
                opts = opts.with_temperature(temp);
            }
            if let Some(max) = ovr.max_tokens {
                opts = opts.with_max_tokens(max);
            }
            if let Some(top_p) = ovr.top_p {
                opts = opts.with_top_p(top_p);
            }
            if let Some(ref effort) = ovr.reasoning_effort {
                opts = opts.with_reasoning_effort(map_reasoning_effort(effort));
                opts = opts.with_capture_reasoning_content(true);
            }
        }

        opts
    }

    fn map_error(e: genai::Error) -> InferenceExecutionError {
        tracing::warn!(error = ?e, "LLM inference error");

        // Try structured matching on status codes first.
        if let Some(status) = Self::extract_status_code(&e) {
            let msg = format!("{e:#}");
            return match status.as_u16() {
                429 => InferenceExecutionError::RateLimited(msg),
                408 | 504 => InferenceExecutionError::Timeout(msg),
                500 | 502 | 503 => InferenceExecutionError::Provider(msg),
                _ => InferenceExecutionError::Provider(msg),
            };
        }

        // Fall back to string matching for errors without structured status codes.
        let msg = format!("{e:#}");
        let lower = msg.to_lowercase();
        if lower.contains("rate") || lower.contains("429") || lower.contains("too many requests") {
            InferenceExecutionError::RateLimited(msg)
        } else if lower.contains("timeout") || lower.contains("timed out") {
            InferenceExecutionError::Timeout(msg)
        } else if lower.contains("overloaded")
            || lower.contains("503")
            || lower.contains("502")
            || lower.contains("500")
        {
            InferenceExecutionError::Provider(msg)
        } else {
            tracing::warn!(error_msg = %msg, "unclassified LLM error — consider adding a pattern");
            InferenceExecutionError::Provider(msg)
        }
    }

    /// Extract an HTTP status code from structured `genai::Error` variants.
    fn extract_status_code(e: &genai::Error) -> Option<StatusCode> {
        match e {
            genai::Error::HttpError { status, .. } => Some(*status),
            genai::Error::WebAdapterCall { webc_error, .. }
            | genai::Error::WebModelCall { webc_error, .. } => {
                if let genai::webc::Error::ResponseFailedStatus { status, .. } = webc_error {
                    Some(*status)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

impl Default for GenaiExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmExecutor for GenaiExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let model = request.model.clone();
        let tools: Vec<_> = request.tools.clone();
        let chat_req = build_chat_request(
            &request.system,
            &request.messages,
            &tools,
            request.enable_prompt_cache,
        );
        let opts = self.build_options(&request);

        let timeout_dur = self.default_timeout;
        let response = tokio::time::timeout(
            timeout_dur,
            self.client.exec_chat(&model, chat_req, Some(&opts)),
        )
        .await
        .map_err(|_| {
            InferenceExecutionError::Timeout(format!(
                "inference timeout after {}s",
                timeout_dur.as_secs()
            ))
        })?
        .map_err(Self::map_error)?;

        // Extract text
        let text = response.content.first_text().unwrap_or("").to_string();

        // Extract tool calls
        let tool_calls: Vec<_> = response
            .content
            .tool_calls()
            .into_iter()
            .map(from_genai_tool_call)
            .collect();

        // Extract usage
        let usage = Some(map_usage(&response.usage));

        // Extract stop reason
        let stop_reason = response.stop_reason.as_ref().and_then(map_stop_reason);

        let content = if text.is_empty() {
            vec![]
        } else {
            vec![ContentBlock::text(text)]
        };

        Ok(StreamResult {
            content,
            tool_calls,
            usage,
            stop_reason,
            has_incomplete_tool_calls: false,
        })
    }

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
            let model = request.model.clone();
            let tools: Vec<_> = request.tools.clone();
            let chat_req = build_chat_request(
                &request.system,
                &request.messages,
                &tools,
                request.enable_prompt_cache,
            );
            let mut opts = self.build_options(&request);
            opts = opts.with_capture_content(true);

            let timeout_dur = self.default_timeout;
            let stream_response = tokio::time::timeout(
                timeout_dur,
                self.client.exec_chat_stream(&model, chat_req, Some(&opts)),
            )
            .await
            .map_err(|_| {
                InferenceExecutionError::Timeout(format!(
                    "inference timeout after {}s",
                    timeout_dur.as_secs()
                ))
            })?
            .map_err(Self::map_error)?;

            let event_stream = futures::stream::unfold(
                (stream_response.stream, StreamCollector::new()),
                |(mut stream, mut collector)| async move {
                    if let Some(output) = collector.take_pending_output() {
                        let event = stream_output_to_llm_event(output)
                            .expect("pending outputs are never empty");
                        return Some((Ok(event), (stream, collector)));
                    }

                    // If we already saw End (emitted Usage on previous poll),
                    // emit the final Stop event now.
                    if collector.end_seen() {
                        let result = collector.finish();
                        let stop = result.stop_reason.unwrap_or(StopReason::EndTurn);
                        return Some((
                            Ok(LlmStreamEvent::Stop(stop)),
                            (stream, StreamCollector::new()),
                        ));
                    }
                    loop {
                        match stream.next().await {
                            Some(Ok(event)) => {
                                let is_end = matches!(event, ChatStreamEvent::End(_));
                                let output = collector.process(event);
                                if let Some(event) = stream_output_to_llm_event(output) {
                                    return Some((Ok(event), (stream, collector)));
                                }
                                if is_end {
                                    // Emit usage event if available, then
                                    // mark end_pending so the next poll
                                    // emits Stop.
                                    if let Some(usage) = collector.take_usage() {
                                        return Some((
                                            Ok(LlmStreamEvent::Usage(usage)),
                                            (stream, collector),
                                        ));
                                    }
                                    let result = collector.finish();
                                    let stop = result.stop_reason.unwrap_or(StopReason::EndTurn);
                                    return Some((
                                        Ok(LlmStreamEvent::Stop(stop)),
                                        (stream, StreamCollector::new()),
                                    ));
                                }
                                continue;
                            }
                            Some(Err(e)) => {
                                return Some((
                                    Err(InferenceExecutionError::Provider(e.to_string())),
                                    (stream, collector),
                                ));
                            }
                            None => return None,
                        }
                    }
                },
            );

            Ok(Box::pin(event_stream) as InferenceStream)
        })
    }

    fn name(&self) -> &str {
        "genai"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::executor::InferenceRequest;
    use awaken_contract::contract::inference::InferenceOverride;
    use awaken_contract::contract::message::Message;

    /// Helper to build a minimal `InferenceRequest` with the given overrides.
    fn make_request(overrides: Option<InferenceOverride>) -> InferenceRequest {
        InferenceRequest {
            model: "test-model".into(),
            messages: vec![Message::user("hello")],
            tools: vec![],
            system: vec![],
            overrides,
            enable_prompt_cache: false,
        }
    }

    // -- Constructor / trait tests --

    #[test]
    fn new_creates_executor() {
        let exec = GenaiExecutor::new();
        assert!(exec.default_options.is_none());
    }

    #[test]
    fn default_creates_executor() {
        let exec = GenaiExecutor::default();
        assert!(exec.default_options.is_none());
    }

    #[test]
    fn name_returns_genai() {
        let exec = GenaiExecutor::new();
        assert_eq!(exec.name(), "genai");
    }

    // -- build_options tests --

    #[test]
    fn build_options_defaults() {
        let exec = GenaiExecutor::new();
        let req = make_request(None);
        let opts = exec.build_options(&req);

        assert_eq!(opts.capture_usage, Some(true));
        assert_eq!(opts.capture_content, Some(true));
        assert_eq!(opts.capture_tool_calls, Some(true));
        assert_eq!(opts.temperature, None);
        assert_eq!(opts.max_tokens, None);
        assert_eq!(opts.top_p, None);
    }

    #[test]
    fn build_options_with_temperature() {
        let exec = GenaiExecutor::new();
        let req = make_request(Some(InferenceOverride {
            temperature: Some(0.5),
            ..Default::default()
        }));
        let opts = exec.build_options(&req);

        assert_eq!(opts.temperature, Some(0.5));
        assert_eq!(opts.max_tokens, None);
        assert_eq!(opts.top_p, None);
    }

    #[test]
    fn build_options_with_max_tokens() {
        let exec = GenaiExecutor::new();
        let req = make_request(Some(InferenceOverride {
            max_tokens: Some(1024),
            ..Default::default()
        }));
        let opts = exec.build_options(&req);

        assert_eq!(opts.max_tokens, Some(1024));
        assert_eq!(opts.temperature, None);
        assert_eq!(opts.top_p, None);
    }

    #[test]
    fn build_options_with_top_p() {
        let exec = GenaiExecutor::new();
        let req = make_request(Some(InferenceOverride {
            top_p: Some(0.9),
            ..Default::default()
        }));
        let opts = exec.build_options(&req);

        assert_eq!(opts.top_p, Some(0.9));
        assert_eq!(opts.temperature, None);
        assert_eq!(opts.max_tokens, None);
    }

    #[test]
    fn build_options_with_all_overrides() {
        let exec = GenaiExecutor::new();
        let req = make_request(Some(InferenceOverride {
            temperature: Some(0.7),
            max_tokens: Some(2048),
            top_p: Some(0.95),
            ..Default::default()
        }));
        let opts = exec.build_options(&req);

        assert_eq!(opts.temperature, Some(0.7));
        assert_eq!(opts.max_tokens, Some(2048));
        assert_eq!(opts.top_p, Some(0.95));
        assert_eq!(opts.capture_usage, Some(true));
        assert_eq!(opts.capture_content, Some(true));
        assert_eq!(opts.capture_tool_calls, Some(true));
    }

    #[test]
    fn build_options_with_default_options() {
        let base = ChatOptions::default()
            .with_temperature(0.3)
            .with_max_tokens(512);
        let exec = GenaiExecutor::new().with_options(base);
        // Override only temperature; max_tokens should come from the executor defaults.
        let req = make_request(Some(InferenceOverride {
            temperature: Some(0.9),
            ..Default::default()
        }));
        let opts = exec.build_options(&req);

        // Per-request override wins for temperature.
        assert_eq!(opts.temperature, Some(0.9));
        // Executor-level default preserved for max_tokens.
        assert_eq!(opts.max_tokens, Some(512));
        // Capture flags still applied.
        assert_eq!(opts.capture_usage, Some(true));
    }

    #[test]
    fn build_options_with_reasoning_effort() {
        let exec = GenaiExecutor::new();
        let req = make_request(Some(InferenceOverride {
            reasoning_effort: Some(ContractReasoningEffort::High),
            ..Default::default()
        }));
        let opts = exec.build_options(&req);

        assert!(
            opts.reasoning_effort.is_some(),
            "reasoning_effort should be set"
        );
        assert_eq!(opts.capture_reasoning_content, Some(true));
    }

    #[test]
    fn build_options_without_reasoning_effort() {
        let exec = GenaiExecutor::new();
        let req = make_request(None);
        let opts = exec.build_options(&req);

        assert!(opts.reasoning_effort.is_none());
        assert!(opts.capture_reasoning_content.is_none());
    }

    #[test]
    fn map_reasoning_effort_all_variants() {
        use super::map_reasoning_effort;

        assert!(matches!(
            map_reasoning_effort(&ContractReasoningEffort::None),
            GenaiReasoningEffort::None
        ));
        assert!(matches!(
            map_reasoning_effort(&ContractReasoningEffort::Low),
            GenaiReasoningEffort::Low
        ));
        assert!(matches!(
            map_reasoning_effort(&ContractReasoningEffort::Medium),
            GenaiReasoningEffort::Medium
        ));
        assert!(matches!(
            map_reasoning_effort(&ContractReasoningEffort::High),
            GenaiReasoningEffort::High
        ));
        assert!(matches!(
            map_reasoning_effort(&ContractReasoningEffort::Max),
            GenaiReasoningEffort::Max
        ));
        assert!(matches!(
            map_reasoning_effort(&ContractReasoningEffort::Budget(4096)),
            GenaiReasoningEffort::Budget(4096)
        ));
    }

    // -- map_error tests --
    //
    // `genai::Error::Internal(String)` is the easiest variant to construct.
    // Its Display is "Internal error: {msg}", so we embed the target substring
    // (e.g. "429", "rate", "timeout") in the message to exercise each branch.

    #[test]
    fn map_error_rate_limited_429() {
        let err = genai::Error::Internal("server returned 429".into());
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::RateLimited(_)),
            "expected RateLimited, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_rate_word() {
        let err = genai::Error::Internal("rate limit exceeded".into());
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::RateLimited(_)),
            "expected RateLimited, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_timeout() {
        let err = genai::Error::Internal("connection timeout".into());
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::Timeout(_)),
            "expected Timeout, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_timed_out() {
        let err = genai::Error::Internal("request timed out".into());
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::Timeout(_)),
            "expected Timeout, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_generic() {
        let err = genai::Error::Internal("something went wrong".into());
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::Provider(_)),
            "expected Provider, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_too_many_requests() {
        let err = genai::Error::Internal("Too Many Requests".into());
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::RateLimited(_)),
            "expected RateLimited, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_overloaded() {
        let err = genai::Error::Internal("server overloaded".into());
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::Provider(_)),
            "expected Provider, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_503_string() {
        let err = genai::Error::Internal("503 Service Unavailable".into());
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::Provider(_)),
            "expected Provider, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_http_429_structured() {
        let err = genai::Error::HttpError {
            status: StatusCode::TOO_MANY_REQUESTS,
            canonical_reason: "Too Many Requests".into(),
            body: "rate limited".into(),
        };
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::RateLimited(_)),
            "expected RateLimited, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_http_500_structured() {
        let err = genai::Error::HttpError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            canonical_reason: "Internal Server Error".into(),
            body: "oops".into(),
        };
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::Provider(_)),
            "expected Provider, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_http_504_structured() {
        let err = genai::Error::HttpError {
            status: StatusCode::GATEWAY_TIMEOUT,
            canonical_reason: "Gateway Timeout".into(),
            body: "timeout".into(),
        };
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::Timeout(_)),
            "expected Timeout, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_preserves_full_chain() {
        let err = genai::Error::Internal("rate limit exceeded".into());
        let mapped = GenaiExecutor::map_error(err);
        let msg = match mapped {
            InferenceExecutionError::RateLimited(m) => m,
            other => panic!("expected RateLimited, got {other:?}"),
        };
        // format!("{e:#}") should give us the full chain
        assert!(msg.contains("rate limit exceeded"), "msg was: {msg}");
    }

    #[test]
    fn with_timeout_builder() {
        let exec = GenaiExecutor::new().with_timeout(Duration::from_secs(30));
        assert_eq!(exec.default_timeout, Duration::from_secs(30));
    }

    // -----------------------------------------------------------------------
    // Inference timeout pattern tests
    // -----------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn timeout_fires_for_slow_future() {
        // Verifies the exact tokio::time::timeout pattern used in GenaiExecutor::execute
        let timeout_dur = Duration::from_secs(120);

        let slow = async {
            tokio::time::sleep(Duration::from_secs(200)).await;
            Ok::<&str, String>("should not reach")
        };

        let result = tokio::time::timeout(timeout_dur, slow).await;
        assert!(result.is_err(), "should have timed out");
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_maps_to_inference_timeout_error() {
        let timeout_dur = Duration::from_millis(50);

        let slow = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<(), ()>(())
        };

        let result = tokio::time::timeout(timeout_dur, slow).await;
        assert!(result.is_err());

        // Verify the mapping pattern used in GenaiExecutor::execute
        let mapped = result.map_err(|_| {
            InferenceExecutionError::Timeout(format!(
                "inference timeout after {}s",
                timeout_dur.as_secs()
            ))
        });
        assert!(
            matches!(mapped, Err(InferenceExecutionError::Timeout(ref msg)) if msg.contains("timeout"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_does_not_fire_for_fast_future() {
        let timeout_dur = Duration::from_secs(120);

        let fast = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok::<&str, String>("done")
        };

        let result = tokio::time::timeout(timeout_dur, fast).await;
        assert!(result.is_ok(), "fast future should not time out");
        assert_eq!(result.unwrap().unwrap(), "done");
    }

    // -----------------------------------------------------------------------
    // Error classification tests for WebAdapterCall and WebModelCall
    // -----------------------------------------------------------------------

    #[test]
    fn map_error_web_adapter_call_429() {
        use genai::adapter::AdapterKind;
        use reqwest::header::HeaderMap;

        let err = genai::Error::WebAdapterCall {
            adapter_kind: AdapterKind::OpenAI,
            webc_error: genai::webc::Error::ResponseFailedStatus {
                status: StatusCode::TOO_MANY_REQUESTS,
                body: "rate limited".into(),
                headers: Box::new(HeaderMap::new()),
            },
        };
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::RateLimited(_)),
            "expected RateLimited, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_web_model_call_429() {
        use genai::ModelIden;
        use genai::adapter::AdapterKind;
        use reqwest::header::HeaderMap;

        let err = genai::Error::WebModelCall {
            model_iden: ModelIden::new(AdapterKind::OpenAI, "gpt-4o"),
            webc_error: genai::webc::Error::ResponseFailedStatus {
                status: StatusCode::TOO_MANY_REQUESTS,
                body: "rate limited".into(),
                headers: Box::new(HeaderMap::new()),
            },
        };
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::RateLimited(_)),
            "expected RateLimited, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_web_adapter_call_503() {
        use genai::adapter::AdapterKind;
        use reqwest::header::HeaderMap;

        let err = genai::Error::WebAdapterCall {
            adapter_kind: AdapterKind::Anthropic,
            webc_error: genai::webc::Error::ResponseFailedStatus {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: "overloaded".into(),
                headers: Box::new(HeaderMap::new()),
            },
        };
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::Provider(_)),
            "expected Provider, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_web_model_call_504() {
        use genai::ModelIden;
        use genai::adapter::AdapterKind;
        use reqwest::header::HeaderMap;

        let err = genai::Error::WebModelCall {
            model_iden: ModelIden::new(AdapterKind::OpenAI, "gpt-4o"),
            webc_error: genai::webc::Error::ResponseFailedStatus {
                status: StatusCode::GATEWAY_TIMEOUT,
                body: "gateway timeout".into(),
                headers: Box::new(HeaderMap::new()),
            },
        };
        let mapped = GenaiExecutor::map_error(err);
        assert!(
            matches!(mapped, InferenceExecutionError::Timeout(_)),
            "expected Timeout, got {mapped:?}"
        );
    }

    #[test]
    fn map_error_web_adapter_call_non_status_error_falls_through() {
        use genai::adapter::AdapterKind;

        // webc::Error that is NOT ResponseFailedStatus — extract_status_code returns None
        let err = genai::Error::WebAdapterCall {
            adapter_kind: AdapterKind::OpenAI,
            webc_error: genai::webc::Error::ResponseFailedNotJson {
                content_type: "text/html".into(),
                body: "not json".into(),
            },
        };
        let mapped = GenaiExecutor::map_error(err);
        // Falls through to string matching → Provider
        assert!(
            matches!(mapped, InferenceExecutionError::Provider(_)),
            "expected Provider, got {mapped:?}"
        );
    }
}
