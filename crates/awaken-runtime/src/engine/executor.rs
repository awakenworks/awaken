//! GenAI-backed LLM executor implementation.

use async_trait::async_trait;
use futures::StreamExt;
use genai::Client;
use genai::chat::{ChatOptions, ChatStreamEvent};

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, InferenceStream, LlmExecutor, LlmStreamEvent,
};
use awaken_contract::contract::inference::{StopReason, StreamResult};

use super::convert::{build_chat_request, from_genai_tool_call, map_stop_reason, map_usage};
use super::streaming::{StreamCollector, StreamOutput};

/// LLM executor backed by the `genai` crate.
///
/// Supports all providers that genai supports: OpenAI, Anthropic, Gemini, Ollama, etc.
/// Configured via environment variables (OPENAI_API_KEY, ANTHROPIC_API_KEY, etc.)
/// or via `genai::ClientConfig`.
pub struct GenaiExecutor {
    client: Client,
    default_options: Option<ChatOptions>,
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
        }
    }

    /// Create with a custom genai `Client`.
    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            default_options: None,
        }
    }

    /// Set default chat options (temperature, max_tokens, etc.).
    #[must_use]
    pub fn with_options(mut self, options: ChatOptions) -> Self {
        self.default_options = Some(options);
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
        }

        opts
    }

    fn map_error(e: genai::Error) -> InferenceExecutionError {
        let msg = e.to_string();
        if msg.contains("rate") || msg.contains("429") {
            InferenceExecutionError::RateLimited(msg)
        } else if msg.contains("timeout") || msg.contains("timed out") {
            InferenceExecutionError::Timeout(msg)
        } else {
            InferenceExecutionError::Provider(msg)
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

        let response = self
            .client
            .exec_chat(&model, chat_req, Some(&opts))
            .await
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

            let stream_response = self
                .client
                .exec_chat_stream(&model, chat_req, Some(&opts))
                .await
                .map_err(Self::map_error)?;

            let event_stream = futures::stream::unfold(
                (stream_response.stream, StreamCollector::new()),
                |(mut stream, mut collector)| async move {
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
                                match output {
                                    StreamOutput::TextDelta(delta) => {
                                        return Some((
                                            Ok(LlmStreamEvent::TextDelta(delta)),
                                            (stream, collector),
                                        ));
                                    }
                                    StreamOutput::ReasoningDelta(delta) => {
                                        return Some((
                                            Ok(LlmStreamEvent::ReasoningDelta(delta)),
                                            (stream, collector),
                                        ));
                                    }
                                    StreamOutput::ToolCallStart { id, name } => {
                                        return Some((
                                            Ok(LlmStreamEvent::ToolCallStart { id, name }),
                                            (stream, collector),
                                        ));
                                    }
                                    StreamOutput::ToolCallDelta { id, args_delta } => {
                                        return Some((
                                            Ok(LlmStreamEvent::ToolCallDelta { id, args_delta }),
                                            (stream, collector),
                                        ));
                                    }
                                    StreamOutput::None => {
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
                                            let stop =
                                                result.stop_reason.unwrap_or(StopReason::EndTurn);
                                            return Some((
                                                Ok(LlmStreamEvent::Stop(stop)),
                                                (stream, StreamCollector::new()),
                                            ));
                                        }
                                        continue;
                                    }
                                }
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
