//! `genai`-backed implementation of the runtime's `LlmExecutor` port.
//!
//! This adapter is the only crate allowed to name the model SDK. It maps the
//! neutral `ChatRequest`/`ChatResponse` onto `genai` types and routes on the
//! selected `ModelBinding.model_ref` — it never picks a different model (G22).

use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::{ContentBlock, ImageSource, extract_text};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, Error, LlmExecutor, Result, StopReason,
    TokenUsage, ToolCall,
};
use genai::Client;
use genai::chat::{
    Binary, ChatMessage, ChatRequest as GenaiChatRequest, ContentPart, MessageContent,
    Tool as GenaiTool, ToolCall as GenaiToolCall, ToolResponse, Usage,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the stream may go silent between events before the turn fails as
/// a retryable timeout. The overall `timeout` only guards opening the call;
/// without this, a provider that stalls mid-stream would hang the run forever.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// A `genai::Client` behind the neutral `LlmExecutor` port.
pub struct GenaiExecutor {
    client: Client,
    timeout: Duration,
    idle_timeout: Duration,
}

impl Default for GenaiExecutor {
    fn default() -> Self {
        Self {
            client: Client::default(),
            timeout: DEFAULT_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }
}

impl GenaiExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a pre-configured client (custom auth, endpoints, adapters).
    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the per-event stall bound for streaming: a stream that produces no
    /// event within this window fails as a retryable timeout.
    #[must_use]
    pub fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    /// An executor pointed at a custom **Anthropic-compatible** endpoint — a
    /// gateway that speaks the Anthropic Messages API (`{base_url}messages`), such
    /// as `https://api.kimi.com/coding/v1/`. Every request routes through the
    /// Anthropic adapter to `base_url`, authenticated by `api_key`; the model name
    /// still comes from the request's `ModelBinding` (G22). This keeps the model
    /// SDK named only here — a consumer passes a base URL, key, and model id.
    pub fn anthropic_compatible(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        use genai::adapter::AdapterKind;
        use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
        use genai::{ModelIden, ServiceTarget};

        let base_url = base_url.into();
        let api_key = api_key.into();
        let resolver = ServiceTargetResolver::from_resolver_fn(
            move |mut target: ServiceTarget| -> std::result::Result<ServiceTarget, genai::resolver::Error> {
                // Force the Anthropic adapter + custom endpoint + key, keeping the
                // caller-selected model name.
                target.endpoint = Endpoint::from_owned(base_url.clone());
                target.auth = AuthData::from_single(api_key.clone());
                target.model =
                    ModelIden::new(AdapterKind::Anthropic, target.model.model_name.clone());
                Ok(target)
            },
        );
        let client = Client::builder()
            .with_service_target_resolver(resolver)
            .build();
        Self::with_client(client)
    }

    /// An executor for **Gemini on Vertex AI**, authenticated by a Google OAuth2
    /// **Bearer token** (e.g. from `gcloud auth print-access-token`, or an ADC /
    /// service-account token). Unlike `anthropic_compatible`, this speaks Gemini's
    /// native `generateContent` wire; genai's Vertex adapter constructs the
    /// `.../projects/{project}/locations/{location}/publishers/google/models/{model}:generateContent`
    /// URL. `location` is a region (`us-central1`) or `global`. The model name still
    /// comes from the request's `ModelBinding` (G22). The OAuth token is short-lived
    /// — build a fresh executor after each refresh.
    pub fn vertex_gemini(
        project: impl Into<String>,
        location: impl Into<String>,
        oauth_token: impl Into<String>,
    ) -> Self {
        use genai::adapter::AdapterKind;
        use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
        use genai::{ModelIden, ServiceTarget};

        let project = project.into();
        let location = location.into();
        let token = oauth_token.into();
        let base_url = if location == "global" {
            format!("https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/")
        } else {
            format!(
                "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/"
            )
        };
        let resolver = ServiceTargetResolver::from_resolver_fn(
            move |mut target: ServiceTarget| -> std::result::Result<ServiceTarget, genai::resolver::Error> {
                // Force the Vertex adapter + project/location endpoint + Bearer OAuth
                // token, keeping the caller-selected Gemini model name.
                target.endpoint = Endpoint::from_owned(base_url.clone());
                target.auth = AuthData::from_single(token.clone());
                target.model = ModelIden::new(AdapterKind::Vertex, target.model.model_name.clone());
                Ok(target)
            },
        );
        let client = Client::builder()
            .with_service_target_resolver(resolver)
            .build();
        Self::with_client(client)
    }
}

#[async_trait]
impl LlmExecutor for GenaiExecutor {
    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse> {
        let model = request.model_binding.model_ref.clone();
        let genai_request = to_genai_request(&request);

        let response = tokio::time::timeout(
            self.timeout,
            self.client.exec_chat(model, genai_request, None),
        )
        .await
        // A timeout is retryable: the same call may succeed on retry.
        .map_err(|_| Error::Timeout("model call timed out".to_string()))?
        .map_err(|err| classify_error(&err.to_string()))?;

        Ok(from_genai_response(response))
    }

    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn awaken_runtime_contract::llm::DeltaSink,
    ) -> Result<ChatResponse> {
        use futures::StreamExt;
        use genai::chat::{ChatOptions, ChatStreamEvent};

        let model = request.model_binding.model_ref.clone();
        let genai_request = to_genai_request(&request);

        // Have genai assemble the committed turn for us. It concatenates the text
        // chunks and parses the accumulated tool-argument fragments into a JSON
        // object, exactly as the non-streaming path returns them — Anthropic
        // streams tool arguments as `input_json_delta` text that is only valid
        // JSON once the block ends. Without `capture_*` the `End` event carries
        // no content and we'd be stuck with the raw, string-encoded deltas.
        let options = ChatOptions::default()
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_usage(true);

        let stream_response = tokio::time::timeout(
            self.timeout,
            self.client
                .exec_chat_stream(model, genai_request, Some(&options)),
        )
        .await
        .map_err(|_| Error::Timeout("model stream timed out".to_string()))?
        .map_err(|err| classify_error(&err.to_string()))?;

        let mut stream = stream_response.stream;
        let mut captured: Option<MessageContent> = None;
        let mut usage: Option<TokenUsage> = None;
        let mut stop_reason: Option<StopReason> = None;
        // Fallback assembly, used only if the provider delivers no captured turn.
        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        loop {
            // Bound the wait for each event: a stream that goes silent without
            // closing must fail as a retryable stall, not hang the run.
            let event = match tokio::time::timeout(self.idle_timeout, stream.next()).await {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(_) => {
                    return Err(Error::Timeout(format!(
                        "model stream stalled: no event within {:?}",
                        self.idle_timeout
                    )));
                }
            };
            match event.map_err(|err| classify_error(&err.to_string()))? {
                // Live text: forward each chunk as it arrives (valid UTF-8 by type).
                ChatStreamEvent::Chunk(chunk) if !chunk.content.is_empty() => {
                    sink.on_text(&chunk.content).await;
                    text.push_str(&chunk.content);
                }
                // Live tool-call progress: genai hands incremental, string-encoded
                // arguments here. Best-effort only; the committed turn is the End
                // event, where genai has parsed the arguments into an object.
                ChatStreamEvent::ToolCallChunk(tool) => {
                    let call = from_genai_tool_call(&tool.tool_call);
                    sink.on_tool_call(&call.call_id, &call.tool_id, &call.arguments)
                        .await;
                    match tool_calls.iter_mut().find(|c| c.call_id == call.call_id) {
                        Some(existing) => *existing = call,
                        None => tool_calls.push(call),
                    }
                }
                // Committed turn: genai's parsed, ordered content and usage.
                ChatStreamEvent::End(end) => {
                    stop_reason = end.captured_stop_reason.as_ref().and_then(map_stop_reason);
                    captured = end.captured_content;
                    usage = end.captured_usage.as_ref().map(map_usage);
                }
                _ => {}
            }
        }

        // Prefer genai's captured turn (parsed tool arguments, same shape as
        // `infer`); fall back to the chunk-assembled blocks only if no End
        // content arrived (e.g. a stream that ends without a captured block).
        let output = match captured {
            Some(content) => map_assistant_output(&content),
            None => {
                let mut blocks: Vec<ContentBlock> = Vec::new();
                if !text.is_empty() {
                    blocks.push(ContentBlock::text(text));
                }
                for call in tool_calls {
                    blocks.push(ContentBlock::tool_use(
                        call.call_id,
                        call.tool_id,
                        call.arguments,
                    ));
                }
                AssistantOutput::from_blocks(blocks)
            }
        };
        Ok(ChatResponse {
            output,
            usage,
            stop_reason,
        })
    }
}

/// Classify a provider error string into the contract's error taxonomy. The
/// SDK surfaces errors as strings, so classification matches lowercase
/// substrings, most-specific class first: a context-overflow 400 must not fall
/// into the generic invalid-request bucket, and a safety rejection must not
/// look like a retryable provider fault. An unmatched message classifies as a
/// retryable `Provider` error — the unknown-failure default mirrors treating
/// an unclassified transport fault as worth one more attempt.
pub fn classify_error(message: &str) -> Error {
    fn hits(lower: &str, needles: &[&str]) -> bool {
        needles.iter().any(|needle| lower.contains(needle))
    }

    let lower = message.to_lowercase();
    // Covers Anthropic/OpenAI/Azure phrasings of "the prompt does not fit".
    const OVERFLOW: &[&str] = &[
        "prompt is too long",
        "context_length_exceeded",
        "context length",
        "input is too long",
        "maximum context length",
        "reduce the length",
        "too many tokens",
        "request too large",
        "413",
    ];
    const CONTENT_FILTER: &[&str] = &["content_filter", "content filter", "content policy"];
    const UNAUTHORIZED: &[&str] = &[
        "401",
        "403",
        "unauthorized",
        "forbidden",
        "invalid api key",
        "authentication",
        "permission denied",
    ];
    const MODEL_NOT_FOUND: &[&str] = &["404", "model not found", "model_not_found"];
    const RATE_LIMITED: &[&str] = &["rate limit", "ratelimit", "429", "too many requests"];
    const OVERLOADED: &[&str] = &["overloaded", "529", "503", "unavailable"];
    const TIMEOUT: &[&str] = &["timeout", "timed out", "408", "504"];
    const INVALID_REQUEST: &[&str] = &["400", "422", "invalid request", "invalid_request_error"];
    // A HARD usage/quota/billing exhaustion — recognised distinctly from a transient
    // 429 rate limit so it is surfaced (not retried) rather than mislabelled.
    const USAGE_LIMIT: &[&str] = &[
        "usage limit",
        "quota",
        "insufficient_quota",
        "billing",
        "credit balance",
        "out of credit",
        "spending limit",
        "weekly limit",
        "monthly limit",
        "exceeded your current",
    ];
    // The credential needs re-authentication (expired grant / login) — recognised
    // distinctly from a generic authorization failure.
    const LOGIN_REQUIRED: &[&str] = &[
        "login required",
        "please log in",
        "session expired",
        "token expired",
        "token has expired",
        "invalid_grant",
        "reauthenticate",
        "re-authenticate",
    ];

    let message = message.to_string();
    if hits(&lower, CONTENT_FILTER) {
        Error::ContentFiltered(message)
    } else if hits(&lower, OVERFLOW) {
        Error::ContextOverflow(message)
    } else if hits(&lower, LOGIN_REQUIRED) {
        Error::LoginRequired(message)
    } else if hits(&lower, UNAUTHORIZED) {
        Error::Unauthorized(message)
    } else if hits(&lower, USAGE_LIMIT) {
        // Recognised only; `reset_after` stays `None` here (a free-text message
        // carries no reliable structured window) and nothing is scheduled from it.
        Error::UsageLimit {
            message,
            reset_after: None,
        }
    } else if hits(&lower, MODEL_NOT_FOUND) {
        Error::ModelNotFound(message)
    } else if hits(&lower, RATE_LIMITED) {
        Error::RateLimited {
            message,
            retry_after: None,
        }
    } else if hits(&lower, OVERLOADED) {
        Error::Overloaded {
            message,
            retry_after: None,
        }
    } else if hits(&lower, TIMEOUT) {
        Error::Timeout(message)
    } else if hits(&lower, INVALID_REQUEST) {
        Error::InvalidRequest(message)
    } else {
        Error::Provider(message)
    }
}

/// The outcome of a live credential probe (ADR-0043 `CredentialValidation`),
/// aligned with the Managed wire's `valid`/`invalid`/`unknown` statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialProbe {
    /// The credential authenticated and the model answered.
    Valid,
    /// The provider rejected the credential (auth/permission failure).
    Invalid,
    /// Inconclusive — a transient/network failure or a non-auth error, so the
    /// credential is neither confirmed nor refuted (never a false `Valid`).
    Unknown,
}

/// Live-probe an Anthropic-compatible LLM credential by making a minimal, one-token
/// request against `base_url` with `api_key` for `model`. A success is `Valid`; a
/// clear authentication/permission rejection is `Invalid`; anything else (rate
/// limit, 5xx, network, bad-model) is `Unknown` — fail-safe, so a flaky endpoint
/// never marks a good key invalid. The secret is used only for this call and never
/// returned.
pub async fn probe_credential(
    base_url: impl Into<String>,
    api_key: impl Into<String>,
    model: &str,
) -> CredentialProbe {
    use awaken_runtime_contract::resolved::ModelBinding;

    let executor = GenaiExecutor::anthropic_compatible(base_url, api_key)
        .with_timeout(Duration::from_secs(30));
    let request = ChatRequest {
        model_binding: ModelBinding {
            provider_instance_ref: "probe".into(),
            model_ref: model.to_string(),
            backend_ref: "genai".into(),
        },
        messages: vec![awaken_runtime_contract::llm::ChatMessage {
            role: ChatRole::User,
            content: vec![ContentBlock::text("ping")],
        }],
        tools: Vec::new(),
    };
    match executor.infer(request).await {
        Ok(_) => CredentialProbe::Valid,
        Err(error) => {
            let message = error.to_string().to_lowercase();
            const AUTH: &[&str] = &[
                "401",
                "403",
                "unauthorized",
                "invalid api key",
                "invalid_api_key",
                "invalid x-api-key",
                "authentication",
                "authentication_error",
                "permission",
                "forbidden",
            ];
            if AUTH.iter().any(|needle| message.contains(needle)) {
                CredentialProbe::Invalid
            } else {
                CredentialProbe::Unknown
            }
        }
    }
}

/// Map the neutral request onto a `genai::ChatRequest`.
pub fn to_genai_request(request: &ChatRequest) -> GenaiChatRequest {
    let mut messages: Vec<ChatMessage> = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        let parts: Vec<ContentPart> = message.content.iter().map(to_genai_part).collect();
        let genai_message = match message.role {
            ChatRole::System => ChatMessage::system(parts),
            ChatRole::Assistant => ChatMessage::assistant(parts),
            ChatRole::User => ChatMessage::user(parts),
            // A tool result must be a tool-role message correlated by tool-call id,
            // or a strict provider rejects the turn ("tool_call_ids did not have
            // response messages").
            ChatRole::Tool => ChatMessage::tool(parts),
        };
        messages.push(genai_message);
    }

    let tools: Vec<GenaiTool> = request
        .tools
        .iter()
        .map(|tool| {
            GenaiTool::new(tool.id.clone())
                .with_description(tool.description.clone())
                .with_schema(tool.parameters.clone())
        })
        .collect();

    let mut genai_request = GenaiChatRequest::new(messages);
    if !tools.is_empty() {
        genai_request = genai_request.with_tools(tools);
    }
    genai_request
}

/// Map one neutral content block onto a `genai` content part. Text maps to text;
/// an image maps to a `Binary` (base64 inline or a URL the provider fetches).
fn to_genai_part(block: &ContentBlock) -> ContentPart {
    match block {
        ContentBlock::Text { text } => ContentPart::Text(text.clone()),
        ContentBlock::Image { source } => ContentPart::Binary(to_genai_binary(source)),
        ContentBlock::ToolUse { id, name, input } => ContentPart::ToolCall(GenaiToolCall {
            call_id: id.clone(),
            fn_name: name.clone(),
            fn_arguments: input.clone(),
            thought_signatures: None,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
        } => ContentPart::ToolResponse(ToolResponse::new(
            tool_use_id.clone(),
            extract_text(content),
        )),
    }
}

fn to_genai_binary(source: &ImageSource) -> Binary {
    match source {
        ImageSource::Base64 { media_type, data } => {
            Binary::from_base64(media_type.clone(), data.clone(), None)
        }
        ImageSource::Url { url } => Binary::from_url(content_type_for_url(url), url.clone(), None),
    }
}

/// A neutral image URL carries no media type; infer one from the extension so
/// the provider gets a concrete MIME type, defaulting to a generic image type.
fn content_type_for_url(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else {
        "image/*"
    }
    .to_string()
}

/// Map a `genai::ToolCall` back into the neutral `ToolCall`.
pub fn from_genai_tool_call(call: &GenaiToolCall) -> ToolCall {
    ToolCall {
        call_id: call.call_id.clone(),
        tool_id: call.fn_name.clone(),
        arguments: call.fn_arguments.clone(),
    }
}

/// Map a `genai::ChatResponse` onto the neutral `ChatResponse`.
pub fn from_genai_response(response: genai::chat::ChatResponse) -> ChatResponse {
    ChatResponse {
        output: map_assistant_output(&response.content),
        usage: Some(map_usage(&response.usage)),
        stop_reason: response.stop_reason.as_ref().and_then(map_stop_reason),
    }
}

/// Map the SDK's stop reason onto the neutral one. A provider-specific reason
/// the SDK cannot classify (`Other`) maps to `None` — unknown, treated by the
/// loop as a natural end.
pub fn map_stop_reason(reason: &genai::chat::StopReason) -> Option<StopReason> {
    use genai::chat::StopReason as GenaiStopReason;
    match reason {
        GenaiStopReason::Completed(_) => Some(StopReason::EndTurn),
        GenaiStopReason::MaxTokens(_) => Some(StopReason::MaxTokens),
        GenaiStopReason::ToolCall(_) => Some(StopReason::ToolUse),
        GenaiStopReason::StopSequence(_) => Some(StopReason::StopSequence),
        GenaiStopReason::ContentFilter(_) => Some(StopReason::ContentFilter),
        GenaiStopReason::Other(_) => None,
    }
}

/// Map a provider turn onto neutral content blocks, preserving the order of text
/// and tool requests so an interleaved turn (text + tool call + text) survives.
pub fn map_assistant_output(content: &MessageContent) -> AssistantOutput {
    let blocks = content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(ContentBlock::text(text.clone())),
            ContentPart::ToolCall(call) => Some(ContentBlock::tool_use(
                call.call_id.clone(),
                call.fn_name.clone(),
                call.fn_arguments.clone(),
            )),
            _ => None,
        })
        .collect();
    AssistantOutput::from_blocks(blocks)
}

pub fn map_usage(usage: &Usage) -> TokenUsage {
    // The prompt-cache breakdown (Anthropic cache_read/cache_creation), when the
    // provider reports it; absent for providers/turns without prompt caching.
    let (cache_read_tokens, cache_creation_tokens) = usage
        .prompt_tokens_details
        .as_ref()
        .map(|d| {
            (
                d.cached_tokens.unwrap_or(0).max(0) as u64,
                d.cache_creation_tokens.unwrap_or(0).max(0) as u64,
            )
        })
        .unwrap_or((0, 0));
    TokenUsage {
        prompt_tokens: usage.prompt_tokens.unwrap_or(0).max(0) as u64,
        completion_tokens: usage.completion_tokens.unwrap_or(0).max(0) as u64,
        cache_read_tokens,
        cache_creation_tokens,
    }
}

#[cfg(test)]
mod classify_tests {
    use super::classify_error;

    #[test]
    fn a_hard_usage_limit_is_recognised_and_not_retryable() {
        for msg in [
            "You have exceeded your current quota",
            "429 insufficient_quota",
            "Your credit balance is too low",
            "weekly limit reached",
            "monthly spending limit exceeded",
        ] {
            let e = classify_error(msg);
            assert_eq!(e.code(), "usage_limit", "for {msg:?}");
            // Recognised, surfaced — never auto-retried (a hard limit cannot clear).
            assert!(!e.is_retryable(), "hard limit must not be retried: {msg:?}");
        }
    }

    #[test]
    fn a_transient_rate_limit_stays_rate_limited_not_usage_limit() {
        let e = classify_error("429 Too Many Requests: rate limit exceeded");
        assert_eq!(e.code(), "rate_limited");
        assert!(e.is_retryable());
    }

    #[test]
    fn a_reauth_signal_is_login_required_before_generic_unauthorized() {
        for msg in [
            "Session expired, please log in",
            "invalid_grant",
            "token has expired",
        ] {
            assert_eq!(classify_error(msg).code(), "login_required", "for {msg:?}");
        }
        // A plain 401 with no re-auth phrasing stays a generic unauthorized.
        assert_eq!(classify_error("401 Unauthorized").code(), "unauthorized");
    }
}
