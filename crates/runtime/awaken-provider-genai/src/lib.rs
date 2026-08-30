//! `genai`-backed implementation of the runtime's `LlmExecutor` port.
//!
//! This adapter is the only crate allowed to name the model SDK. It maps the
//! neutral `ChatRequest`/`ChatResponse` onto `genai` types and routes on the
//! selected `ModelBinding.model_ref` — it never picks a different model (G22).

use std::time::{Duration, Instant};

use async_trait::async_trait;
use awaken_agent_contract::agent::content::{
    ContentBlock, DocumentSource, ImageSource, extract_text,
};
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, DEFAULT_MODEL_RESPONSE_TIMEOUT, Error, LlmExecutor,
    Result, StopReason, TokenUsage, ToolCall,
};
use genai::Client;
use genai::chat::{
    Binary, ChatMessage, ChatRequest as GenaiChatRequest, ContentPart, MessageContent,
    ThinkingBlock, Tool as GenaiTool, ToolCall as GenaiToolCall, ToolResponse, Usage,
};

/// The genai wire adapter, re-exported so a consumer selects a provider wire without
/// naming the model SDK itself (which stays named only in this crate).
pub use genai::adapter::AdapterKind;

mod anthropic_content;
mod inference_options;
mod model_discovery;
pub use model_discovery::{ModelDiscoveryError, discover_model_ids};
mod openai_responses;
pub use openai_responses::OpenAiResponsesExecutor;
mod transcript_projection;

use transcript_projection::{
    NeutralPartKind, ProviderPartKind, ReasoningFoldAction, ReplayRowState, ResponseTransport,
    ThinkingProjection, TranscriptDialect, decide_reasoning_fold, project_part_kind,
    project_thinking,
};

// One model response can contain a long reasoning prelude followed by a large typed
// tool call. Keep that complete-response budget independent from the per-event
// silence bound: using the same 120-second value for both made healthy streams
// from reasoning models fail while they were still emitting progress.
/// How long the stream may go silent between events before the response fails as
/// a retryable timeout. `timeout` independently bounds the complete streaming
/// inference, including opening and consuming the response; without both bounds,
/// either a silent stream or an endless stream of no-op events could hang a run.
// Strong reasoning models can legitimately produce no SSE event for more than
// one minute before their first visible token. Keep the default aligned with
// the call-open timeout so that this valid prefill/reasoning interval is not
// misclassified as a stalled transport. Tests and specialized hosts can still
// choose a tighter bound with `with_idle_timeout`.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

#[cfg(test)]
mod default_timeout_tests {
    use super::DEFAULT_IDLE_TIMEOUT;
    use awaken_runtime_contract::llm::{
        DEFAULT_MODEL_ATTEMPT_WATCHDOG, DEFAULT_MODEL_RESPONSE_TIMEOUT,
    };

    /// Cause/effect design: C1 the default complete-response and idle-silence
    /// budgets are selected together. Effect E1: the complete response retains
    /// at least five idle windows, so healthy reasoning progress is not bounded
    /// by the transport-silence threshold. Coverage rule T1=C1=>E1; a smaller
    /// ratio is the regression boundary asserted here.
    /// Constraints/invariants: the complete-call budget must remain at least
    /// five times the idle window; specialized overrides do not change defaults.
    #[test]
    fn complete_reasoning_response_has_a_larger_budget_than_transport_silence() {
        assert!(DEFAULT_MODEL_RESPONSE_TIMEOUT >= DEFAULT_IDLE_TIMEOUT * 5);
        assert!(DEFAULT_MODEL_RESPONSE_TIMEOUT < DEFAULT_MODEL_ATTEMPT_WATCHDOG);
    }
}

fn normalize_provider_base_url(adapter: AdapterKind, base_url: String) -> String {
    if adapter == AdapterKind::Anthropic {
        // genai concatenates `messages` for this adapter instead of URL-joining
        // it, so a custom path prefix must retain its trailing separator.
        format!("{}/", base_url.trim_end_matches('/'))
    } else {
        base_url
    }
}

/// A `genai::Client` behind the neutral `LlmExecutor` port.
pub struct GenaiExecutor {
    client: Client,
    adapter: Option<AdapterKind>,
    unspecified_reasoning: awaken_runtime_contract::UnspecifiedReasoning,
    timeout: Duration,
    idle_timeout: Duration,
}

impl GenaiExecutor {
    /// Use an explicitly configured client. Product composition must prefer
    /// [`Self::from_materialized_endpoint`]; this constructor exists for adapters/tests with
    /// another explicit `ServiceTargetResolver`. There is intentionally no
    /// `Default`/`new` path because the SDK default reads ambient provider env.
    pub fn with_client(client: Client) -> Self {
        Self::with_client_and_adapter(client, None)
    }

    /// Construct from a custom client while retaining the provider dialect
    /// needed to replay provider-specific assistant continuation blocks.
    pub fn with_client_for_adapter(client: Client, adapter: AdapterKind) -> Self {
        Self::with_client_and_adapter(client, Some(adapter))
    }

    fn with_client_and_adapter(client: Client, adapter: Option<AdapterKind>) -> Self {
        Self {
            client,
            adapter,
            unspecified_reasoning: Default::default(),
            timeout: DEFAULT_MODEL_RESPONSE_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
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

    /// The single materialized transport path: inject the caller-supplied
    /// authentication value, force the declared `adapter` (keeping the request's
    /// model name, G22), and use the exact endpoint already selected by the
    /// publication/materialization boundary.
    ///
    /// Every provider served by `genai` routes through this function. API key,
    /// OAuth, broker, project, region, and default-endpoint decisions are already
    /// reflected in the two opaque materialized strings; this adapter does not
    /// receive or reconstruct those control-plane facts.
    pub fn from_materialized_endpoint(
        adapter: genai::adapter::AdapterKind,
        base_url: impl Into<String>,
        authentication: impl Into<String>,
    ) -> Self {
        Self::from_materialized_endpoint_with_reasoning(
            adapter,
            base_url,
            authentication,
            awaken_runtime_contract::UnspecifiedReasoning::ProviderDefault,
        )
    }

    fn from_materialized_endpoint_with_reasoning(
        adapter: genai::adapter::AdapterKind,
        base_url: impl Into<String>,
        authentication: impl Into<String>,
        unspecified_reasoning: awaken_runtime_contract::UnspecifiedReasoning,
    ) -> Self {
        use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
        use genai::{ModelIden, ServiceTarget};

        let authentication = authentication.into();
        let base_url = normalize_provider_base_url(adapter, base_url.into());
        let resolver = ServiceTargetResolver::from_resolver_fn(
            move |mut target: ServiceTarget| -> std::result::Result<ServiceTarget, genai::resolver::Error> {
                target.auth = AuthData::from_single(authentication.clone());
                target.model = ModelIden::new(adapter, target.model.model_name.clone());
                target.endpoint = Endpoint::from_owned(base_url.clone());
                Ok(target)
            },
        );
        let client = Client::builder()
            .with_service_target_resolver(resolver)
            .build();
        let mut executor = Self::with_client_for_adapter(client, adapter);
        executor.unspecified_reasoning = unspecified_reasoning;
        executor
    }
}

#[async_trait]
impl LlmExecutor for GenaiExecutor {
    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse> {
        self.infer_non_streaming(request, None).await
    }

    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn awaken_runtime_contract::llm::DeltaSink,
    ) -> Result<ChatResponse> {
        use futures::StreamExt;
        use genai::chat::ChatStreamEvent;

        let model = request.model_binding.model_ref.clone();
        let genai_request = to_genai_request_with_adapter(&request, self.adapter)?;

        // Have genai assemble the committed response for us. It concatenates the text
        // chunks and parses the accumulated tool-argument fragments into a JSON
        // object, exactly as the non-streaming path returns them — Anthropic
        // streams tool arguments as `input_json_delta` text that is only valid
        // JSON once the block ends. Without `capture_*` the `End` event carries
        // no content and we'd be stuck with the raw, string-encoded deltas.
        let options = self
            .chat_options(&request, true)?
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_usage(true)
            // Capture the model's reasoning so a thinking-capable provider's
            // extended-thinking folds into a `Thinking` block (→ `agent.thinking`).
            .with_capture_reasoning_content(true);

        let started = Instant::now();
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
        // Fallback assembly, used only if the provider delivers no captured response.
        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        // Per-call bytes already emitted: genai hands CUMULATIVE argument snapshots,
        // so this adapter is the single owner that de-accumulates them into suffix
        // deltas (`ToolCallDelta`); no downstream ever diffs again.
        let mut tool_sent: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let deadline = started + self.timeout;
        let mut progress_deadline = Instant::now() + self.idle_timeout;

        loop {
            // Two independent bounds share the one configured call timeout.
            // The fixed deadline caps the complete reasoning response; the
            // progress deadline ignores transport/no-op activity and requires
            // actual text, reasoning, tool progress, or a terminal event. This
            // prevents provider heartbeats and empty choices from occupying the
            // full reasoning budget while retaining one stream parser/authority.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout(
                    "model stream exceeded total timeout".to_string(),
                ));
            }
            let progress_remaining = progress_deadline.saturating_duration_since(Instant::now());
            if progress_remaining.is_zero() {
                return Err(Error::Timeout(format!(
                    "model stream stalled: no useful event within {:?}",
                    self.idle_timeout
                )));
            }
            let wait = progress_remaining.min(remaining);
            let event = match tokio::time::timeout(wait, stream.next()).await {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(_) if Instant::now() >= deadline => {
                    return Err(Error::Timeout(
                        "model stream exceeded total timeout".to_string(),
                    ));
                }
                Err(_) => {
                    return Err(Error::Timeout(format!(
                        "model stream stalled: no useful event within {:?}",
                        self.idle_timeout
                    )));
                }
            };
            let mut made_progress = false;
            match event.map_err(|err| classify_error(&err.to_string()))? {
                // Live text: forward each chunk as it arrives (valid UTF-8 by type).
                ChatStreamEvent::Chunk(chunk) if !chunk.content.is_empty() => {
                    sink.on_text(&chunk.content).await;
                    text.push_str(&chunk.content);
                    made_progress = true;
                }
                // Live tool-call progress: genai hands incremental, string-encoded
                // arguments here. Best-effort only; the committed response is the End
                // event, where genai has parsed the arguments into an object.
                ChatStreamEvent::ToolCallChunk(tool) => {
                    let call = from_genai_tool_call(&tool.tool_call);
                    let is_new_call = !tool_calls.iter().any(|c| c.call_id == call.call_id);
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
                            made_progress = true;
                        }
                    }
                    made_progress |= is_new_call;
                    match tool_calls.iter_mut().find(|c| c.call_id == call.call_id) {
                        Some(existing) => *existing = call,
                        None => tool_calls.push(call),
                    }
                }
                // Live reasoning: announce progress on the distinct reasoning
                // channel; Managed exposes only a start marker and never the text.
                // The completed form still becomes the committed `Thinking` block.
                ChatStreamEvent::ReasoningChunk(chunk) => {
                    if !chunk.content.is_empty() {
                        sink.on_reasoning(&chunk.content).await;
                        reasoning.push_str(&chunk.content);
                        made_progress = true;
                    }
                }
                // Committed response: genai's parsed, ordered content and usage.
                ChatStreamEvent::End(end) => {
                    stop_reason = end
                        .captured_stop_reason
                        .as_ref()
                        .map(map_stop_reason)
                        .transpose()?;
                    captured = end.captured_content;
                    usage = end.captured_usage.as_ref().map(map_usage);
                    if let Some(r) = end.captured_reasoning_content {
                        reasoning = r;
                    }
                    // `End` is the model protocol's authoritative turn boundary.
                    // OpenAI-compatible gateways may keep the HTTP connection
                    // alive after `[DONE]`; waiting for transport EOF in that
                    // case leaves an otherwise complete run stuck until the
                    // idle timeout. Do not let connection reuse redefine model
                    // completion.
                    break;
                }
                _ => {}
            }
            if made_progress {
                progress_deadline = Instant::now() + self.idle_timeout;
            }
        }

        // Prefer genai's captured response (parsed tool arguments, same shape as
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
        // Fold the response's reasoning through the same canonical response mapper
        // used by non-streaming inference so both paths commit an identical Step response.
        let output = with_reasoning(output, Some(&reasoning), ResponseTransport::Streaming);
        require_usable_response(ChatResponse {
            output,
            usage,
            stop_reason,
        })
    }
}

impl GenaiExecutor {
    async fn infer_non_streaming(
        &self,
        request: ChatRequest,
        max_tokens_override: Option<u32>,
    ) -> Result<ChatResponse> {
        let model = request.model_binding.model_ref.clone();
        let genai_request = to_genai_request_with_adapter(&request, self.adapter)?;
        let mut options = self.chat_options(&request, false)?;
        if let Some(max_tokens) = max_tokens_override {
            options = options.with_max_tokens(max_tokens);
        }

        let response = tokio::time::timeout(
            self.timeout,
            self.client.exec_chat(model, genai_request, Some(&options)),
        )
        .await
        // A timeout is retryable: the same call may succeed on retry.
        .map_err(|_| Error::Timeout("model call timed out".to_string()))?
        .map_err(|err| classify_error(&err.to_string()))?;

        require_usable_response(from_genai_response(response)?)
    }

    fn chat_options(
        &self,
        request: &ChatRequest,
        streaming: bool,
    ) -> Result<genai::chat::ChatOptions> {
        inference_options::materialize(request, streaming, self.unspecified_reasoning)
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
        "insufficient balance",
        "insufficient_balance",
        "payment required",
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

/// Total tri-state reduction after the transport adapter has classified whether
/// a failed request is an authentication rejection. Success always wins; every
/// non-authentication failure remains inconclusive rather than retiring a key.
#[must_use]
pub const fn credential_probe_outcome(
    succeeded: bool,
    authentication_rejected: bool,
) -> CredentialProbe {
    if succeeded {
        CredentialProbe::Valid
    } else if authentication_rejected {
        CredentialProbe::Invalid
    } else {
        CredentialProbe::Unknown
    }
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

    let executor =
        GenaiExecutor::from_materialized_endpoint(AdapterKind::Anthropic, base_url, api_key)
            .with_timeout(Duration::from_secs(30));
    let request = ChatRequest {
        model_binding: ModelBinding {
            provider_identity_ref: "probe".into(),
            model_ref: model.to_string(),
            backend_ref: "genai".into(),
        },
        inference: Default::default(),
        messages: vec![awaken_runtime_contract::llm::ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::text("ping")],
        }],
        tools: Vec::new(),
    };
    match executor.infer_non_streaming(request, Some(1)).await {
        Ok(_) => credential_probe_outcome(true, false),
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
            credential_probe_outcome(false, AUTH.iter().any(|needle| message.contains(needle)))
        }
    }
}

/// Map the neutral request onto a `genai::ChatRequest`.
pub fn to_genai_request(request: &ChatRequest) -> Result<GenaiChatRequest> {
    to_genai_request_with_adapter(request, None)
}

/// Map the neutral request using an explicit provider wire dialect.
pub fn to_genai_request_for_adapter(
    request: &ChatRequest,
    adapter: AdapterKind,
) -> Result<GenaiChatRequest> {
    to_genai_request_with_adapter(request, Some(adapter))
}

fn to_genai_request_with_adapter(
    request: &ChatRequest,
    adapter: Option<AdapterKind>,
) -> Result<GenaiChatRequest> {
    let mut messages: Vec<ChatMessage> = Vec::with_capacity(request.messages.len());
    let dialect = transcript_dialect(adapter);
    for message in &request.messages {
        let row_state = message
            .content
            .iter()
            .fold(ReplayRowState::Empty, |state, block| {
                state.absorb(project_part_kind(dialect, neutral_part_kind(block)))
            });
        if !row_state.should_replay() {
            continue;
        }
        let parts: Vec<ContentPart> = message
            .content
            .iter()
            .map(|block| to_genai_part(block, dialect))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        // A standalone reasoning-only history row is not a complete assistant
        // message and some provider protocols reject it. Reasoning that accompanies
        // text or a tool call remains attached and is replayed by adapters that
        // require it (notably DeepSeek's OpenAI-compatible tool loop).
        // The neutral transcript commits one Tool message per completed call.
        // Anthropic Messages instead requires every result for one assistant
        // tool-use message to appear together in the immediately following user
        // message. `genai` maps one Tool message to one Anthropic user message,
        // so coalesce adjacent Tool messages here. OpenAI/Responses still emit
        // one wire result per part from the combined message.
        if message.role == Role::Tool
            && let Some(previous) = messages.last_mut()
            && previous.role == genai::chat::ChatRole::Tool
        {
            previous.content.extend(parts);
            continue;
        }
        let genai_message = match message.role {
            Role::System => ChatMessage::system(parts),
            Role::Assistant => ChatMessage::assistant(parts),
            Role::User => ChatMessage::user(parts),
            // A tool result must be a tool-role message correlated by tool-call id,
            // or a strict provider rejects the message ("tool_call_ids did not have
            // response messages").
            Role::Tool => ChatMessage::tool(parts),
        };
        messages.push(genai_message);
    }

    if let Some(tool) = request
        .tools
        .iter()
        .find(|tool| tool.provider_server_tool.is_some())
    {
        return Err(Error::Binding(format!(
            "provider-server tool `{}` requires an exact native adapter",
            tool.id
        )));
    }
    let tools: Vec<GenaiTool> = request
        .tools
        .iter()
        .map(|tool| {
            GenaiTool::new(tool.id.clone())
                .with_description(tool.description.clone())
                .with_schema(tool.model_parameters())
        })
        .collect();

    let mut genai_request = GenaiChatRequest::new(messages);
    if !tools.is_empty() {
        genai_request = genai_request.with_tools(tools);
    }
    Ok(genai_request)
}

/// Map one neutral content block onto a `genai` content part. Text maps to text;
/// an image maps to a `Binary` (base64 inline or a URL the provider fetches).
fn to_genai_part(block: &ContentBlock, dialect: TranscriptDialect) -> Result<Option<ContentPart>> {
    match (project_part_kind(dialect, neutral_part_kind(block)), block) {
        (ProviderPartKind::Text, ContentBlock::Text { text }) => {
            Ok(Some(ContentPart::Text(text.clone())))
        }
        (
            ProviderPartKind::Text,
            ContentBlock::SearchResult {
                source,
                title,
                content,
                ..
            },
        ) => Ok(Some(ContentPart::Text(format!(
            "{title}\nSource: {source}\n{}",
            content
                .iter()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        )))),
        (ProviderPartKind::Text, ContentBlock::ToolReference { tool_name }) => Ok(Some(
            ContentPart::Text(format!("Deferred tool `{tool_name}` is now available.")),
        )),
        (ProviderPartKind::Binary, ContentBlock::Image { source }) => {
            Ok(Some(ContentPart::Binary(to_genai_binary(source)?)))
        }
        (
            ProviderPartKind::Binary,
            ContentBlock::Document {
                source,
                title,
                context,
            },
        ) if dialect == TranscriptDialect::Anthropic => Ok(Some(ContentPart::from_custom(
            anthropic_content::document(source, title.as_deref(), context.as_deref())?,
            None,
        ))),
        (ProviderPartKind::Binary, ContentBlock::Document { source, .. }) => {
            let part = match source {
                DocumentSource::Base64 { media_type, data } => {
                    ContentPart::Binary(Binary::from_base64(media_type.clone(), data.clone(), None))
                }
                DocumentSource::Text { data, .. } => ContentPart::Text(data.clone()),
                DocumentSource::Url { url } => ContentPart::Binary(Binary::from_url(
                    "application/octet-stream",
                    url.clone(),
                    None,
                )),
                DocumentSource::File { file_id } => {
                    return Err(Error::InvalidRequest(format!(
                        "File {file_id} was not materialized before provider dispatch"
                    )));
                }
            };
            Ok(Some(part))
        }
        (ProviderPartKind::ToolCall, ContentBlock::ToolUse { id, name, input }) => {
            Ok(Some(ContentPart::ToolCall(GenaiToolCall {
                call_id: id.clone(),
                fn_name: name.clone(),
                fn_arguments: input.clone(),
                thought_signatures: None,
            })))
        }
        (
            ProviderPartKind::ToolResponse,
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            },
        ) if dialect == TranscriptDialect::Anthropic => Ok(Some(ContentPart::from_custom(
            serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content
                    .iter()
                    .map(anthropic_content::tool_result_content)
                    .collect::<Result<Vec<_>>>()?,
                "is_error": is_error,
            }),
            None,
        ))),
        (
            ProviderPartKind::ToolResponse,
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            },
        ) => Ok(Some(ContentPart::ToolResponse(ToolResponse::new(
            tool_use_id.clone(),
            extract_text(content),
        )))),
        (
            ProviderPartKind::Reasoning | ProviderPartKind::SignedThinking,
            ContentBlock::Thinking { text, signature },
        ) => Ok(Some(
            match project_thinking(dialect, text.clone(), signature.clone()) {
                ThinkingProjection::Signed {
                    thinking,
                    signature,
                } => ContentPart::Thinking(ThinkingBlock::new(thinking, signature)),
                ThinkingProjection::Reasoning(reasoning) => {
                    ContentPart::ReasoningContent(reasoning)
                }
            },
        )),
        (ProviderPartKind::Omitted, ContentBlock::Redacted) => Ok(None),
        _ => unreachable!("closed transcript projection returned a mismatched part kind"),
    }
}

fn transcript_dialect(adapter: Option<AdapterKind>) -> TranscriptDialect {
    if adapter == Some(AdapterKind::Anthropic) {
        TranscriptDialect::Anthropic
    } else {
        TranscriptDialect::Other
    }
}

const fn neutral_part_kind(block: &ContentBlock) -> NeutralPartKind {
    match block {
        ContentBlock::Text { .. } => NeutralPartKind::Text,
        ContentBlock::Image { .. } => NeutralPartKind::Image,
        ContentBlock::Document { .. } => NeutralPartKind::Document,
        ContentBlock::SearchResult { .. } => NeutralPartKind::SearchResult,
        ContentBlock::ToolReference { .. } => NeutralPartKind::ToolReference,
        ContentBlock::Redacted => NeutralPartKind::Redacted,
        ContentBlock::ToolUse { .. } => NeutralPartKind::ToolUse,
        ContentBlock::ToolResult { .. } => NeutralPartKind::ToolResult,
        ContentBlock::Thinking { .. } => NeutralPartKind::Thinking,
    }
}

fn to_genai_binary(source: &ImageSource) -> Result<Binary> {
    Ok(match source {
        ImageSource::Base64 { media_type, data } => {
            Binary::from_base64(media_type.clone(), data.clone(), None)
        }
        ImageSource::Url { url } => Binary::from_url(content_type_for_url(url), url.clone(), None),
        ImageSource::File { file_id } => {
            return Err(Error::InvalidRequest(format!(
                "File {file_id} was not materialized before provider dispatch"
            )));
        }
    })
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
pub fn from_genai_response(response: genai::chat::ChatResponse) -> Result<ChatResponse> {
    let output = with_reasoning(
        map_assistant_output(&response.content),
        response.reasoning_content.as_deref(),
        ResponseTransport::NonStreaming,
    );
    let stop_reason = response
        .stop_reason
        .as_ref()
        .map(map_stop_reason)
        .transpose()?;
    Ok(ChatResponse {
        output,
        usage: Some(map_usage(&response.usage)),
        stop_reason,
    })
}

fn require_usable_response(response: ChatResponse) -> Result<ChatResponse> {
    if response.output.text_content().trim().is_empty() && response.output.tool_calls().is_empty() {
        // A reasoning model can spend its entire output allowance before a
        // public token or typed tool call fits. Preserve only the explicitly
        // truncated form so Runtime's one bounded MaxTokens continuation owner
        // can ask for smaller pieces. A NaturalEnd reasoning-only response is
        // still unusable and follows the retry/failure path below.
        if response.stop_reason == Some(StopReason::MaxTokens) && response.output.has_reasoning() {
            return Ok(response);
        }
        return Err(Error::Provider(
            "model returned no visible assistant content or tool call".into(),
        ));
    }
    // Some OpenAI-compatible reasoning providers occasionally serialize their
    // private tool-call wire language as assistant text. Committing that text as
    // a successful response silently drops the requested side effect. Do not parse
    // or execute it here: only the SDK's typed ToolCall is trusted. A retryable
    // provider error lets the canonical run retry policy obtain a structured
    // response without creating a second provider-specific tool parser.
    if response.output.tool_calls().is_empty()
        && response
            .output
            .text_content()
            .contains("<｜｜DSML｜｜tool_calls>")
    {
        return Err(Error::Provider(
            "model serialized a tool call as assistant text instead of structured tool data".into(),
        ));
    }
    Ok(response)
}

#[cfg(test)]
mod visible_response_tests {
    use super::*;

    #[test]
    fn reasoning_only_response_distinguishes_truncation_from_empty_success() {
        // Test design — Causes: C1 a provider returns private reasoning and
        // usage but no public text or typed tool call; C2 stop reason is absent,
        // NaturalEnd, or MaxTokens; C3 reasoning is useful, absent, or blank.
        // Effects: E1 every non-truncation and empty/blank response is retryable;
        // E2 only useful reasoning+MaxTokens reaches Runtime's bounded
        // continuation owner. Constraints/invariants: reasoning never becomes
        // answer text or ordinary success. Rules V1=C1+useful+NaturalEnd=>E1;
        // V2=C1+useful+MaxTokens=>E2; V3=C1+(absent|blank)+any stop=>E1.
        let response = |reasoning: Option<&str>, stop_reason| ChatResponse {
            output: AssistantOutput::from_blocks(
                reasoning.map(ContentBlock::thinking).into_iter().collect(),
            ),
            usage: Some(TokenUsage {
                completion_tokens: 7,
                ..TokenUsage::default()
            }),
            stop_reason,
        };

        let error =
            require_usable_response(response(Some("reasoning"), Some(StopReason::NaturalEnd)))
                .unwrap_err();
        assert!(matches!(error, Error::Provider(_)));
        assert!(error.is_retryable());
        let truncated =
            require_usable_response(response(Some("reasoning"), Some(StopReason::MaxTokens)))
                .expect("V2/E2");
        assert!(truncated.output.has_reasoning(), "V2/E2");
        assert_eq!(truncated.stop_reason, Some(StopReason::MaxTokens), "V2/E2");
        for reasoning in [None, Some(""), Some("   ")] {
            for stop_reason in [
                None,
                Some(StopReason::NaturalEnd),
                Some(StopReason::MaxTokens),
            ] {
                let error = require_usable_response(response(reasoning, stop_reason))
                    .expect_err("V3/E1 blank private output is never usable");
                assert!(matches!(error, Error::Provider(_)), "V3/E1");
                assert!(error.is_retryable(), "V3/E1");
            }
        }
    }

    /// Textual-tool-call cause/effect graph, decision table, and FMECA. Causes:
    /// C1 typed tool calls exist; C2 assistant text has the unambiguous DSML
    /// tool-call sentinel. Effects: E1 accept typed or ordinary text; E2 reject
    /// serialized tool syntax as a retryable provider fault without executing
    /// it. Rules T1=C1=>E1, T2=!C1&C2=>E2, and T3=!C1&!C2=>E1. FMECA: committing
    /// serialized tool syntax is critical because an expected external effect
    /// never occurs while the Session looks successful; this sole provider seam
    /// detects it even on the tool-free reserved final step, and deliberately
    /// avoids a second, injection-prone tool parser.
    /// Constraints/invariants: only structured provider tool data is executable;
    /// sentinel text is detected but never parsed as a second tool authority.
    #[test]
    fn textual_dsml_tool_calls_always_fail_closed() {
        let serialized = || ChatResponse {
            output: AssistantOutput::text(
                "<｜｜DSML｜｜tool_calls> <｜｜DSML｜｜invoke name=\"publish\">",
            ),
            usage: None,
            stop_reason: Some(StopReason::NaturalEnd),
        };

        let error = require_usable_response(serialized()).expect_err("T2/E2");
        assert!(matches!(error, Error::Provider(_)), "T2/E2");
        assert!(error.is_retryable(), "T2/E2");

        let ordinary = ChatResponse {
            output: AssistantOutput::text("ordinary final answer"),
            usage: None,
            stop_reason: Some(StopReason::NaturalEnd),
        };
        assert!(require_usable_response(ordinary).is_ok(), "T3/E1");

        let typed = ChatResponse {
            output: AssistantOutput::from_blocks(vec![
                ContentBlock::text("<｜｜DSML｜｜tool_calls>"),
                ContentBlock::tool_use("call-1", "publish", serde_json::json!({})),
            ]),
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
        };
        assert!(require_usable_response(typed).is_ok(), "T1/E1");
    }
}

/// Map the SDK's stop reason onto the neutral one. A provider-specific reason
/// the SDK cannot classify (`Other`) fails at this adapter boundary instead of
/// being erased into `None` and mistaken for a natural end by Runtime.
pub fn map_stop_reason(reason: &genai::chat::StopReason) -> Result<StopReason> {
    use genai::chat::StopReason as GenaiStopReason;
    match reason {
        GenaiStopReason::Completed(_) => Ok(StopReason::NaturalEnd),
        GenaiStopReason::MaxTokens(_) => Ok(StopReason::MaxTokens),
        GenaiStopReason::ToolCall(_) => Ok(StopReason::ToolUse),
        GenaiStopReason::StopSequence(_) => Ok(StopReason::StopSequence),
        GenaiStopReason::ContentFilter(_) => Ok(StopReason::ContentFilter),
        GenaiStopReason::Other(_) => Err(Error::Provider(
            "model returned an unclassified stop reason".into(),
        )),
    }
}

/// Map a provider response onto neutral content blocks, preserving the order of text
/// and tool requests so an interleaved response (text + tool call + text) survives.
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
            ContentPart::ReasoningContent(reasoning) => {
                Some(ContentBlock::thinking(reasoning.clone()))
            }
            ContentPart::Thinking(thinking) => Some(ContentBlock::signed_thinking(
                thinking.thinking.clone(),
                thinking.signature.clone(),
            )),
            _ => None,
        })
        .collect();
    AssistantOutput::from_blocks(blocks)
}

/// Add provider-normalized scalar reasoning only when ordered content did not
/// already carry a Thinking block. Ordered blocks are the replay authority;
/// the scalar is a compatibility projection and must never duplicate them.
fn with_reasoning(
    mut output: AssistantOutput,
    reasoning: Option<&str>,
    transport: ResponseTransport,
) -> AssistantOutput {
    let reasoning = reasoning.filter(|reasoning| !reasoning.trim().is_empty());
    let has_ordered_thinking = output
        .blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Thinking { .. }));
    if matches!(
        decide_reasoning_fold(transport, reasoning.is_some(), has_ordered_thinking),
        ReasoningFoldAction::Prepend
    ) {
        output.blocks.insert(
            0,
            ContentBlock::thinking(reasoning.expect("prepend requires non-blank reasoning")),
        );
    }
    output
}

#[cfg(test)]
mod reasoning_projection_tests {
    use super::*;

    fn public_response() -> AssistantOutput {
        AssistantOutput::from_blocks(vec![
            ContentBlock::tool_use("call-1", "read_fixture", serde_json::json!({"id": 1})),
            ContentBlock::text("public answer"),
        ])
    }

    #[test]
    fn streaming_and_non_streaming_commit_the_same_reasoning_shape() {
        // Test design — Causes: identical public output and private reasoning
        // arrive through streaming and non-streaming transports. Effects: both
        // commit the same ordered Thinking/ToolUse/Text blocks. Constraints/
        // invariants: transport cannot alter committed transcript semantics.
        // Decision rule R1: equal inputs across both modes=>identical block vector.
        let streaming = with_reasoning(
            public_response(),
            Some("private reasoning"),
            ResponseTransport::Streaming,
        );
        let non_streaming = with_reasoning(
            public_response(),
            Some("private reasoning"),
            ResponseTransport::NonStreaming,
        );

        assert_eq!(streaming.blocks, non_streaming.blocks);
        assert!(matches!(
            streaming.blocks.first(),
            Some(ContentBlock::Thinking { text, .. }) if text == "private reasoning"
        ));
        assert_eq!(streaming.tool_calls().len(), 1);
        assert_eq!(streaming.text_content(), "public answer");
    }

    #[test]
    fn exact_embedded_reasoning_is_not_duplicated_or_reordered() {
        let output = AssistantOutput::from_blocks(vec![
            ContentBlock::thinking("private reasoning"),
            ContentBlock::tool_use("call-1", "read_fixture", serde_json::json!({"id": 1})),
        ]);
        let folded = with_reasoning(
            output,
            Some("private reasoning"),
            ResponseTransport::NonStreaming,
        );

        assert_eq!(
            folded
                .blocks
                .iter()
                .filter(|block| matches!(block, ContentBlock::Thinking { .. }))
                .count(),
            1
        );
        assert!(matches!(
            folded.blocks.first(),
            Some(ContentBlock::Thinking { text, .. }) if text == "private reasoning"
        ));
        assert_eq!(folded.tool_calls().len(), 1);
    }

    #[test]
    fn ordered_signed_thinking_blocks_a_different_scalar_without_losing_order() {
        // Test design — Causes: C1 output already contains two ordered signed
        // Thinking blocks interleaved with public text/tool use; C2 a different
        // aggregate reasoning scalar arrives from streaming transport. Effect:
        // C1 remains byte/order exact and C2 is ignored rather than duplicated.
        // Constraints/invariants: signed structured blocks are authoritative;
        // the lossy scalar may fill only an output with no Thinking block.
        // Decision rule R1=C1+C2=>preserve all blocks/signatures exactly.
        let output = AssistantOutput::from_blocks(vec![
            ContentBlock::signed_thinking("first", Some("opaque-signature-1".into())),
            ContentBlock::text("public"),
            ContentBlock::signed_thinking("second", Some("opaque-signature-2".into())),
            ContentBlock::tool_use("call-1", "read_fixture", serde_json::json!({"id": 1})),
        ]);
        let expected = output.blocks.clone();

        let folded = with_reasoning(
            output,
            Some("different aggregate scalar"),
            ResponseTransport::Streaming,
        );

        assert_eq!(folded.blocks, expected);
        assert!(matches!(
            &folded.blocks[0],
            ContentBlock::Thinking { text, signature }
                if text == "first" && signature.as_deref() == Some("opaque-signature-1")
        ));
        assert!(matches!(
            &folded.blocks[2],
            ContentBlock::Thinking { text, signature }
                if text == "second" && signature.as_deref() == Some("opaque-signature-2")
        ));
    }
}

pub fn map_usage(usage: &Usage) -> TokenUsage {
    // The prompt-cache breakdown (Anthropic cache_read/cache_creation), when the
    // provider reports it; absent for providers/responses without prompt caching.
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
    use std::time::Duration;

    use super::{DEFAULT_IDLE_TIMEOUT, classify_error};

    #[test]
    fn default_stream_idle_window_allows_long_reasoning_prefill() {
        // Test design — Causes: valid model prefill may remain silent for sixty
        // seconds. Effects: the default idle window remains at least 120 seconds.
        // Constraints/invariants: silence timeout must not misclassify that known
        // healthy interval. Decision rule D1: default>=120s=>prefill boundary safe.
        // A live DeepSeek reasoning response produced no SSE event for just over
        // sixty seconds and was incorrectly terminated as a transport stall.
        // Preserve at least two minutes for valid model-side prefill/reasoning;
        // explicit test/host overrides retain the fast-stall path.
        assert!(DEFAULT_IDLE_TIMEOUT >= Duration::from_secs(120));
    }

    #[test]
    fn a_hard_usage_limit_is_recognised_and_not_retryable() {
        for msg in [
            "You have exceeded your current quota",
            "429 insufficient_quota",
            r#"402 Payment Required: {"error":{"message":"Insufficient Balance","type":"unknown_error","code":"invalid_request_error"}}"#,
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

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn credential_probe_tri_state_is_total_and_fail_safe() {
        let succeeded = kani::any::<bool>();
        let authentication_rejected = kani::any::<bool>();
        let outcome = credential_probe_outcome(succeeded, authentication_rejected);
        if succeeded {
            assert_eq!(outcome, CredentialProbe::Valid);
        } else if authentication_rejected {
            assert_eq!(outcome, CredentialProbe::Invalid);
        } else {
            assert_eq!(outcome, CredentialProbe::Unknown);
        }
    }

    #[kani::proof]
    fn unclassified_provider_stop_reason_fails_closed() {
        let result = map_stop_reason(&genai::chat::StopReason::Other(String::new()));
        assert!(matches!(result, Err(Error::Provider(_))));
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

    use super::{
        AdapterKind, CredentialProbe, GenaiExecutor, discover_model_ids,
        normalize_provider_base_url, probe_credential, to_genai_request,
    };

    #[test]
    fn anthropic_gateway_base_preserves_its_path_separator() {
        // Causes: exact endpoint uses Anthropic concatenation or another adapter's
        // ordinary URL handling. Effects: Anthropic receives one trailing slash;
        // other adapters receive the exact materialized URL. Rules N1/N2 cover
        // both adapter partitions without selecting a provider default.
        assert_eq!(
            normalize_provider_base_url(
                AdapterKind::Anthropic,
                "https://api.deepseek.com/anthropic".into(),
            ),
            "https://api.deepseek.com/anthropic/"
        );
        assert_eq!(
            normalize_provider_base_url(AdapterKind::OpenAI, "https://api.deepseek.com".into()),
            "https://api.deepseek.com"
        );
    }

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
                    // Keep the connection open; `message_stop` already ended the response.
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

    /// Return one fixed JSON response and retain the exact HTTP request. The
    /// credential-probe contract test uses this instead of a mock at the neutral
    /// port so the final provider wire, including `max_tokens`, is exercised.
    async fn spawn_capturing_status_server(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, std::sync::Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds");
        let addr = listener.local_addr().expect("addr");
        let requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accepts probe request");
            let mut buf = [0u8; 8192];
            let read = socket.read(&mut buf).await.expect("reads probe request");
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..read]).into_owned());
            let response = format!(
                "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("writes probe response");
        });
        (format!("http://{addr}/"), requests)
    }

    async fn spawn_json_pages(
        pages: Vec<&'static str>,
    ) -> (String, std::sync::Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds");
        let addr = listener.local_addr().expect("addr");
        let requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        tokio::spawn(async move {
            for body in pages {
                let (mut socket, _) = listener.accept().await.expect("accepts page request");
                let mut buf = [0u8; 8192];
                let read = socket.read(&mut buf).await.expect("reads request");
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..read]).into_owned());
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("writes page");
            }
        });
        (format!("http://{addr}/v1"), requests)
    }

    fn request_has_header(request: &str, expected_name: &str, expected_value: &str) -> bool {
        request.lines().skip(1).any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case(expected_name) && value.trim() == expected_value
            })
        })
    }

    #[tokio::test]
    async fn anthropic_model_discovery_reads_every_page_and_never_returns_a_partial_list() {
        let (base, requests) = spawn_json_pages(vec![
            r#"{"data":[{"id":"model-b"}],"has_more":true,"last_id":"model-b"}"#,
            r#"{"data":[{"id":"model-a"}],"has_more":false,"last_id":"model-a"}"#,
        ])
        .await;
        let models = discover_model_ids(AdapterKind::Anthropic, &base, "private-key")
            .await
            .unwrap();
        assert_eq!(models, vec!["model-a", "model-b"]);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("GET /v1/models?limit=1000 "));
        assert!(request_has_header(&requests[0], "x-api-key", "private-key"));
        assert!(requests[1].contains("after_id=model-b"));
    }

    #[tokio::test]
    async fn gemini_model_discovery_normalizes_names_and_carries_page_tokens() {
        let (base, requests) = spawn_json_pages(vec![
            r#"{"models":[{"name":"models/gemini-b"}],"nextPageToken":"next"}"#,
            r#"{"models":[{"name":"models/gemini-a"}]}"#,
        ])
        .await;
        let models = discover_model_ids(AdapterKind::Gemini, &base, "private-key")
            .await
            .unwrap();
        assert_eq!(models, vec!["gemini-a", "gemini-b"]);
        let requests = requests.lock().unwrap();
        assert!(requests[0].contains("key=private-key"));
        assert!(requests[1].contains("pageToken=next"));
    }

    #[tokio::test]
    async fn model_discovery_rejects_an_incomplete_pagination_contract() {
        let (base, _) =
            spawn_json_pages(vec![r#"{"data":[{"id":"partial"}],"has_more":true}"#]).await;
        let error = discover_model_ids(AdapterKind::Anthropic, &base, "private-key")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no last_id"));
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

    #[test]
    fn compatible_chat_adapter_rejects_provider_server_tools() {
        // Decision rule: provider-server projection + generic compatible Chat
        // adapter -> fail before dispatch; only its exact native adapter may
        // lower this descriptor instead of silently turning it into a function.
        let request = ChatRequest {
            model_binding: ModelBinding::new("route", "model", "native"),
            inference: Default::default(),
            messages: Vec::new(),
            tools: vec![weather_tool().with_provider_server_tool(
                awaken_runtime_contract::resolved::ProviderServerTool::openrouter_tool_search(None),
            )],
        };
        assert!(
            to_genai_request(&request)
                .unwrap_err()
                .to_string()
                .contains("requires an exact native adapter")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_tool_args_de_accumulate_into_suffix_deltas() {
        // Test design — Causes: an Anthropic SSE stream sends cumulative tool
        // argument snapshots split across multibyte characters. Effects: live
        // output emits suffix deltas that concatenate to the committed JSON tool
        // call. Constraints/invariants: the adapter is the sole de-accumulation
        // owner and slices only UTF-8 boundaries. Decision rule A1: cumulative
        // snapshots=>exact suffix sequence, one identical committed object.
        let base_url = spawn_sse_server(tool_stream_body()).await;
        let executor =
            GenaiExecutor::from_materialized_endpoint(AdapterKind::Anthropic, base_url, "test-key")
                .with_idle_timeout(Duration::from_secs(5));

        let request = ChatRequest {
            model_binding: ModelBinding {
                provider_identity_ref: "p".to_string(),
                model_ref: "claude-test".to_string(),
                backend_ref: "b".to_string(),
            },
            inference: Default::default(),
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
        .expect("a well-formed tool stream is a response, not an error");

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
        // No text was streamed on a pure tool response.
        assert!(recorder.text.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_credential_classifies_http_401_as_invalid() {
        // Probe cause/effect decision table: P1=2xx -> Valid; P2=401/403 auth
        // rejection -> Invalid; P3=transport/5xx/bad endpoint -> Unknown. FMECA:
        // false Valid admits a bad credential (critical), false Invalid can retire
        // a good one during outage (high); exact P2 classification and P3 fail-safe
        // prevent both. This test owns P2; the next owns P3; the ignored live test
        // owns P1 through the same canonical Anthropic ACL.
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn credential_probe_uses_one_token_and_no_tools_on_the_provider_wire() {
        // Cause/effect decision table:
        // C1 a credential probe is requested; C2 the selected endpoint answers
        // successfully. E1 exactly one output token is requested; E2 no tool is
        // exposed; E3 the tiny probe prompt is the only message; E4 success is
        // Valid. Constraint: normal inference options are unchanged because the
        // bound is applied only by the canonical non-streaming executor path's
        // probe override. Rule P1=C1+C2=>E1+E2+E3+E4.
        let response = r#"{"id":"msg_probe","type":"message","role":"assistant","content":[{"type":"text","text":"p"}],"model":"claude-test","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}"#;
        let (base_url, requests) = spawn_capturing_status_server("HTTP/1.1 200 OK", response).await;

        let outcome = probe_credential(base_url, "probe-secret", "claude-test").await;
        assert_eq!(outcome, CredentialProbe::Valid, "P1/E4");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "one bounded probe request");
        let (_, body) = requests[0]
            .split_once("\r\n\r\n")
            .expect("HTTP request has a body");
        let request: serde_json::Value = serde_json::from_str(body).expect("JSON probe body");
        assert_eq!(request["max_tokens"], 1, "P1/E1");
        assert_eq!(
            request["messages"].as_array().map(Vec::len),
            Some(1),
            "P1/E3"
        );
        assert_eq!(request["messages"][0]["content"], "ping", "P1/E3");
        assert!(request.get("tools").is_none(), "P1/E2");
        assert!(
            !body.contains("probe-secret"),
            "secret stays in the auth header"
        );
    }
}
