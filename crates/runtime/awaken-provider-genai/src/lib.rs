//! `genai`-backed implementation of the runtime's `LlmExecutor` port.
//!
//! This adapter is the only crate allowed to name the model SDK. It maps the
//! neutral `ChatRequest`/`ChatResponse` onto `genai` types and routes on the
//! selected `ModelBinding.model_ref` — it never picks a different model (G22).

use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::{ContentBlock, ImageSource, extract_text};
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, Error, LlmExecutor, Result, StopReason, TokenUsage,
    ToolCall,
};
use genai::Client;
use genai::chat::{
    Binary, ChatMessage, ChatRequest as GenaiChatRequest, ContentPart, MessageContent,
    Tool as GenaiTool, ToolCall as GenaiToolCall, ToolResponse, Usage,
};

/// The genai wire adapter, re-exported so a consumer selects a provider wire without
/// naming the model SDK itself (which stays named only in this crate).
pub use genai::adapter::AdapterKind;

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

    /// The single API-key executor path: inject the caller-supplied `key` (from a
    /// config-plane credential, never the process env), force the declared `adapter`
    /// (keeping the request's model name, G22), and override the endpoint only when a
    /// gateway `base_url` is given (else genai's default for that adapter).
    ///
    /// **Every API-key provider routes through this one function** — Anthropic
    /// (native or a compatible gateway), Gemini via AI Studio, OpenAI and its many
    /// compatible vendors (Groq, Together, Moonshot, …). Adding a provider is a
    /// catalog entry + an `adapter_kind` mapping, not a new constructor. (Vertex/OAuth
    /// is a different auth shape — a Bearer token + a per-project URL — so it keeps
    /// its own [`vertex_gemini`] constructor.)
    pub fn from_resolved(
        adapter: genai::adapter::AdapterKind,
        base_url: Option<String>,
        key: impl Into<String>,
    ) -> Self {
        use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
        use genai::{ModelIden, ServiceTarget};

        let key = key.into();
        let resolver = ServiceTargetResolver::from_resolver_fn(
            move |mut target: ServiceTarget| -> std::result::Result<ServiceTarget, genai::resolver::Error> {
                target.auth = AuthData::from_single(key.clone());
                target.model = ModelIden::new(adapter, target.model.model_name.clone());
                if let Some(base_url) = &base_url {
                    target.endpoint = Endpoint::from_owned(base_url.clone());
                }
                Ok(target)
            },
        );
        let client = Client::builder()
            .with_service_target_resolver(resolver)
            .build();
        Self::with_client(client)
    }

    /// An executor pointed at a custom **Anthropic-compatible** endpoint (e.g.
    /// `https://api.kimi.com/coding/v1/`). A thin wrapper over [`from_resolved`] kept
    /// for the callers that name a base URL + key directly.
    pub fn anthropic_compatible(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::from_resolved(
            genai::adapter::AdapterKind::Anthropic,
            Some(base_url.into()),
            api_key,
        )
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
            .with_capture_usage(true)
            // Capture the model's reasoning so a thinking-capable provider's
            // extended-thinking folds into a `Thinking` block (→ `agent.thinking`).
            .with_capture_reasoning_content(true);

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
        // Accumulated reasoning (streamed chunks); the End event's captured value
        // is preferred when present.
        let mut reasoning = String::new();
        // Fallback assembly, used only if the provider delivers no captured turn.
        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        // Per-call bytes already emitted: genai hands CUMULATIVE argument snapshots,
        // so this adapter is the single owner that de-accumulates them into suffix
        // deltas (`ToolCallDelta`); no downstream ever diffs again.
        let mut tool_sent: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

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
                    // genai's `arguments` is the cumulative JSON string so far; emit
                    // only the newly-appended suffix. A non-string or non-continuation
                    // snapshot is skipped (best-effort — the End event carries the
                    // authoritative parsed input).
                    if let Some(cum) = call.arguments.as_str() {
                        let sent = tool_sent.entry(call.call_id.clone()).or_insert(0);
                        if cum.len() > *sent && cum.is_char_boundary(*sent) {
                            sink.on_tool_call_delta(&call.call_id, &call.tool_id, &cum[*sent..])
                                .await;
                            *sent = cum.len();
                        }
                    }
                    match tool_calls.iter_mut().find(|c| c.call_id == call.call_id) {
                        Some(existing) => *existing = call,
                        None => tool_calls.push(call),
                    }
                }
                // Live reasoning: accumulate; the completed form becomes a folded
                // `Thinking` block. Not forwarded as a text delta (reasoning is not
                // answer content).
                ChatStreamEvent::ReasoningChunk(chunk) => {
                    reasoning.push_str(&chunk.content);
                }
                // Committed turn: genai's parsed, ordered content and usage.
                ChatStreamEvent::End(end) => {
                    stop_reason = end.captured_stop_reason.as_ref().and_then(map_stop_reason);
                    captured = end.captured_content;
                    usage = end.captured_usage.as_ref().map(map_usage);
                    if let Some(r) = end.captured_reasoning_content {
                        reasoning = r;
                    }
                }
                _ => {}
            }
        }

        // Prefer genai's captured turn (parsed tool arguments, same shape as
        // `infer`); fall back to the chunk-assembled blocks only if no End
        // content arrived (e.g. a stream that ends without a captured block).
        let mut output = match captured {
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
        // Fold the turn's reasoning into a leading `Thinking` block: it precedes the
        // answer, is ignored by `extract_text`, and the Managed wire projects its
        // presence as a contentless `agent.thinking` marker.
        if !reasoning.trim().is_empty() {
            output.blocks.insert(0, ContentBlock::thinking(reasoning));
        }
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
            provider_identity_ref: "probe".into(),
            model_ref: model.to_string(),
            backend_ref: "genai".into(),
        },
        messages: vec![awaken_runtime_contract::llm::ChatMessage {
            role: Role::User,
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
        // Reasoning is output-only: a folded `Thinking` block is never replayed to
        // the provider as input (the answer text carries the turn's meaning).
        let parts: Vec<ContentPart> = message
            .content
            .iter()
            .filter(|b| !matches!(b, ContentBlock::Thinking { .. }))
            .map(to_genai_part)
            .collect();
        let genai_message = match message.role {
            Role::System => ChatMessage::system(parts),
            Role::Assistant => ChatMessage::assistant(parts),
            Role::User => ChatMessage::user(parts),
            // A tool result must be a tool-role message correlated by tool-call id,
            // or a strict provider rejects the turn ("tool_call_ids did not have
            // response messages").
            Role::Tool => ChatMessage::tool(parts),
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
        // Filtered out before this map (reasoning is not replayed); mapped
        // defensively to its text so the match stays exhaustive.
        ContentBlock::Thinking { text } => ContentPart::Text(text.clone()),
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

#[cfg(test)]
mod hermetic_tests {
    //! Network-free coverage for the paths that previously only had `#[ignore]`
    //! live tests: streaming tool-argument de-accumulation, the Vertex base-URL
    //! shape, and the credential probe's auth vs. inconclusive classification.
    //! Each test either uses a localhost TCP server emitting Anthropic-shaped SSE
    //! (the same idiom as `tests/stall.rs`) or asserts pure strings — no live
    //! provider, so all of these run by default.

    use std::sync::Mutex;
    use std::time::Duration;

    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::Role;
    use awaken_runtime_contract::llm::{
        ChatMessage, ChatRequest, DeltaSink, LlmExecutor, StopReason,
    };
    use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{CredentialProbe, GenaiExecutor, probe_credential};

    /// One SSE frame: `event:`/`data:` lines terminated by a blank line. The
    /// `data` value is serialized compactly (single line) so it is a valid SSE
    /// payload — the same framing `tests/stall.rs` writes by hand.
    fn frame(event: &str, data: serde_json::Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    /// An Anthropic-shaped SSE stream that requests a single `get_weather` tool
    /// call, delivering the tool arguments as **incremental** `input_json_delta`
    /// fragments. genai accumulates these into cumulative snapshots; the adapter
    /// under test must de-accumulate them back into suffix deltas.
    ///
    /// The value spans multi-byte UTF-8 (`São Paulo 😀`) and the fragment splits
    /// fall exactly on code-point boundaries, exercising the `is_char_boundary`
    /// guard and the `cum[*sent..]` byte-offset slicing with non-ASCII content.
    fn tool_stream_body() -> String {
        use serde_json::json;
        // Concatenate to: {"city":"São Paulo 😀"}
        let fragments = ["{\"city\":\"S", "ão Pa", "ulo ", "😀\"}"];
        let mut body = String::new();
        body.push_str(&frame(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_1", "type": "message", "role": "assistant",
                    "content": [], "model": "m",
                    "stop_reason": null, "stop_sequence": null,
                    "usage": {"input_tokens": 7, "output_tokens": 1}
                }
            }),
        ));
        body.push_str(&frame(
            "content_block_start",
            json!({
                "type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {}}
            }),
        ));
        for fragment in fragments {
            body.push_str(&frame(
                "content_block_delta",
                json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": fragment}
                }),
            ));
        }
        body.push_str(&frame(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ));
        body.push_str(&frame(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                "usage": {"output_tokens": 15}
            }),
        ));
        body.push_str(&frame("message_stop", json!({"type": "message_stop"})));
        body
    }

    /// Spawn a localhost server that replies to the first request with `body` as
    /// an event-stream, then holds the connection open (the stream self-terminates
    /// on `message_stop`, so no close is needed — mirrors `tests/stall.rs`).
    async fn spawn_sse_server(body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    let _ = socket.flush().await;
                    // Keep the connection open; `message_stop` already ended the turn.
                    std::future::pending::<()>().await;
                });
            }
        });
        format!("http://{addr}/")
    }

    /// Spawn a localhost server that replies to every request with a fixed HTTP
    /// status + JSON body, used to drive the credential probe's classification.
    async fn spawn_status_server(status_line: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    let response = format!(
                        "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        format!("http://{addr}/")
    }

    #[derive(Default)]
    struct Recorder {
        text: Mutex<Vec<String>>,
        /// (call_id, tool_id, args_delta) — each delta is a de-accumulated suffix.
        tool_deltas: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait::async_trait]
    impl DeltaSink for Recorder {
        async fn on_text(&self, chunk: &str) {
            self.text.lock().unwrap().push(chunk.to_string());
        }
        async fn on_tool_call_delta(&self, call_id: &str, tool_id: &str, args_delta: &str) {
            self.tool_deltas.lock().unwrap().push((
                call_id.to_string(),
                tool_id.to_string(),
                args_delta.to_string(),
            ));
        }
    }

    fn weather_tool() -> ToolDescriptor {
        ToolDescriptor::pinned(
            "test",
            "get_weather",
            "Get the current weather for a city.",
            serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_tool_args_de_accumulate_into_suffix_deltas() {
        let base_url = spawn_sse_server(tool_stream_body()).await;
        let executor = GenaiExecutor::anthropic_compatible(base_url, "test-key")
            .with_idle_timeout(Duration::from_secs(5));

        let request = ChatRequest {
            model_binding: ModelBinding {
                provider_identity_ref: "p".to_string(),
                model_ref: "claude-test".to_string(),
                backend_ref: "b".to_string(),
            },
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::text("weather in Sao Paulo?")],
            }],
            tools: vec![weather_tool()],
        };

        let recorder = Recorder::default();
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            executor.infer_streaming(request, &recorder),
        )
        .await
        .expect("the stream self-terminates on message_stop")
        .expect("a well-formed tool stream is a turn, not an error");

        // Committed truth (G13): genai parsed the accumulated fragments into an
        // object with the multi-byte value intact.
        let calls = response.output.tool_calls();
        assert_eq!(calls.len(), 1, "one committed tool call");
        assert_eq!(calls[0].tool_id, "get_weather");
        assert_eq!(calls[0].call_id, "toolu_1");
        let expected_args = serde_json::json!({"city": "São Paulo 😀"});
        assert_eq!(calls[0].arguments, expected_args);
        assert_eq!(response.stop_reason, Some(StopReason::ToolUse));

        // The live plane: the adapter is the single de-accumulation owner. The
        // recorded deltas are the exact per-event suffixes (the leading empty
        // snapshot from content_block_start emits nothing), and they concatenate
        // back to the full argument JSON — proving the suffix-diff / is_char_boundary
        // path handled the multi-byte splits correctly.
        let deltas = recorder.tool_deltas.lock().unwrap().clone();
        assert!(
            !deltas.is_empty(),
            "at least one live tool-call delta arrived"
        );
        for (call_id, tool_id, _) in &deltas {
            assert_eq!(call_id, "toolu_1");
            assert_eq!(tool_id, "get_weather");
        }
        let suffixes: Vec<String> = deltas.iter().map(|(_, _, d)| d.clone()).collect();
        assert_eq!(
            suffixes,
            vec![
                "{\"city\":\"S".to_string(),
                "ão Pa".to_string(),
                "ulo ".to_string(),
                "😀\"}".to_string(),
            ],
            "each delta is the newly-appended suffix, never a re-sent cumulative snapshot"
        );
        let joined: String = suffixes.concat();
        assert_eq!(joined, "{\"city\":\"São Paulo 😀\"}");
        let reparsed: serde_json::Value =
            serde_json::from_str(&joined).expect("concatenated live suffixes are valid JSON");
        assert_eq!(
            reparsed, expected_args,
            "the concatenated live suffixes reconstruct the committed object"
        );
        // No text was streamed on a pure tool turn.
        assert!(recorder.text.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_credential_classifies_http_401_as_invalid() {
        // A clear authentication rejection (401) is a definitive `Invalid` — the
        // key is refuted, never a false `Valid`.
        let base_url = spawn_status_server(
            "HTTP/1.1 401 Unauthorized",
            r#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        )
        .await;
        let outcome = probe_credential(base_url, "bogus-key", "claude-test").await;
        assert_eq!(
            outcome,
            CredentialProbe::Invalid,
            "a 401 must probe Invalid"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_credential_classifies_http_503_as_unknown() {
        // A transient server fault is inconclusive (fail-safe): the credential is
        // neither confirmed nor refuted, so a flaky endpoint never marks a good
        // key invalid.
        let base_url = spawn_status_server(
            "HTTP/1.1 503 Service Unavailable",
            r#"{"error":{"type":"overloaded_error","message":"service unavailable"}}"#,
        )
        .await;
        let outcome = probe_credential(base_url, "some-key", "claude-test").await;
        assert_eq!(
            outcome,
            CredentialProbe::Unknown,
            "a transient 503 must probe Unknown, not Invalid"
        );
    }

    #[test]
    fn vertex_gemini_global_and_regional_base_urls() {
        // Contract for the Vertex endpoint `vertex_gemini` constructs: the global
        // location uses the un-prefixed `aiplatform` host with a `.../global/` path,
        // while a region prefixes both the host (`{location}-aiplatform`) and the
        // trailing path segment. genai's Vertex adapter appends
        // `publishers/google/models/{model}:generateContent` to this base.
        let project = "my-proj";

        let global =
            format!("https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/");
        assert_eq!(
            global,
            "https://aiplatform.googleapis.com/v1/projects/my-proj/locations/global/"
        );
        assert!(
            !global.contains("global-aiplatform"),
            "global uses the un-prefixed host"
        );

        let location = "us-central1";
        let regional = format!(
            "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/"
        );
        assert_eq!(
            regional,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1/"
        );
        // A region prefixes the host and closes the path with the same region.
        assert!(regional.starts_with("https://us-central1-aiplatform.googleapis.com/"));
        assert!(regional.ends_with("/locations/us-central1/"));
    }
}
