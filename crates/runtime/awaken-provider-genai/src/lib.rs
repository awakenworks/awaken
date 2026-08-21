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
    AssistantOutput, ChatRequest, ChatResponse, Error, LlmExecutor, Result, StopReason, TokenUsage,
    ToolCall,
};
use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};
use genai::Client;
use genai::chat::{
    Binary, ChatMessage, ChatRequest as GenaiChatRequest, ContentPart, MessageContent,
    ThinkingBlock, Tool as GenaiTool, ToolCall as GenaiToolCall, ToolResponse, Usage,
};
use genai::chat::{ChatOptions, ReasoningEffort as GenaiReasoningEffort};

/// The genai wire adapter, re-exported so a consumer selects a provider wire without
/// naming the model SDK itself (which stays named only in this crate).
pub use genai::adapter::AdapterKind;

mod anthropic_content;
mod openai_responses;
pub use openai_responses::OpenAiResponsesExecutor;
mod transcript_projection;

use transcript_projection::{
    NeutralPartKind, ProviderPartKind, ReasoningFoldAction, ReplayRowState, ResponseTransport,
    ThinkingProjection, TranscriptDialect, decide_reasoning_fold, project_part_kind,
    project_thinking,
};

/// Failure to obtain a complete provider model listing. Callers must not
/// reconcile a partial response because doing so could falsely mark offerings
/// unavailable.
#[derive(Debug, thiserror::Error)]
pub enum ModelDiscoveryError {
    #[error("model discovery is unsupported for adapter {0:?}")]
    Unsupported(AdapterKind),
    #[error("model discovery endpoint is invalid: {0}")]
    InvalidEndpoint(String),
    #[error("model discovery request failed: {0}")]
    Transport(String),
    #[error("model discovery returned HTTP {status}")]
    Http { status: u16 },
    #[error("model discovery response is invalid: {0}")]
    InvalidResponse(String),
}

/// Fetch the complete model directory exposed by a provider API. This adapter
/// performs transport/protocol work only: it neither selects credentials nor
/// writes the management-plane catalog. The already-materialized key exists only
/// for these requests and the result is a normalized, secret-free list of ids.
pub async fn discover_model_ids(
    adapter: AdapterKind,
    base_url: Option<&str>,
    api_key: &str,
) -> std::result::Result<Vec<String>, ModelDiscoveryError> {
    let base = match (adapter, base_url) {
        (AdapterKind::Anthropic, None) => "https://api.anthropic.com/v1",
        (AdapterKind::OpenAI, None) => "https://api.openai.com/v1",
        (AdapterKind::Gemini, None) => "https://generativelanguage.googleapis.com/v1beta",
        (AdapterKind::Vertex, None) => {
            return Err(ModelDiscoveryError::InvalidEndpoint(
                "Vertex model discovery requires the authored project/location base URL".into(),
            ));
        }
        (_, Some(base)) => base,
        (other, None) => return Err(ModelDiscoveryError::Unsupported(other)),
    };
    let mut url = reqwest::Url::parse(base)
        .map_err(|error| ModelDiscoveryError::InvalidEndpoint(error.to_string()))?;
    let model_path = if adapter == AdapterKind::Vertex {
        "publishers/google/models"
    } else {
        "models"
    };
    if !url.path().trim_end_matches('/').ends_with(model_path) {
        let path = format!("{}/{model_path}", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| ModelDiscoveryError::Transport(error.to_string()))?;
    let mut ids = std::collections::BTreeSet::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::BTreeSet::new();
    loop {
        let mut page_url = url.clone();
        {
            let mut query = page_url.query_pairs_mut();
            match adapter {
                AdapterKind::Anthropic => {
                    query.append_pair("limit", "1000");
                    if let Some(cursor) = &cursor {
                        query.append_pair("after_id", cursor);
                    }
                }
                AdapterKind::Gemini | AdapterKind::Vertex => {
                    query.append_pair("pageSize", "1000");
                    if let Some(cursor) = &cursor {
                        query.append_pair("pageToken", cursor);
                    }
                    if adapter == AdapterKind::Gemini {
                        query.append_pair("key", api_key);
                    }
                }
                _ => {}
            }
        }
        let mut request = client.get(page_url);
        request = match adapter {
            AdapterKind::Anthropic => request
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01"),
            AdapterKind::OpenAI | AdapterKind::Vertex => request.bearer_auth(api_key),
            AdapterKind::Gemini => request,
            other => return Err(ModelDiscoveryError::Unsupported(other)),
        };
        let response = request
            .send()
            .await
            .map_err(|error| ModelDiscoveryError::Transport(error.without_url().to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| ModelDiscoveryError::Transport(error.without_url().to_string()))?;
        if !status.is_success() {
            return Err(ModelDiscoveryError::Http {
                status: status.as_u16(),
            });
        }
        let value: serde_json::Value = serde_json::from_str(&body)
            .map_err(|error| ModelDiscoveryError::InvalidResponse(error.to_string()))?;
        let entries = match adapter {
            AdapterKind::Anthropic | AdapterKind::OpenAI => value.get("data"),
            AdapterKind::Gemini | AdapterKind::Vertex => value.get("models"),
            other => return Err(ModelDiscoveryError::Unsupported(other)),
        }
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ModelDiscoveryError::InvalidResponse("missing model array".into()))?;
        for entry in entries {
            let raw = entry
                .get("id")
                .or_else(|| entry.get("name"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ModelDiscoveryError::InvalidResponse("model has no id/name".into())
                })?;
            let id = raw.strip_prefix("models/").unwrap_or(raw).trim();
            if !id.is_empty() {
                ids.insert(id.to_string());
            }
        }
        let next_cursor = match adapter {
            AdapterKind::Anthropic
                if value.get("has_more").and_then(serde_json::Value::as_bool) == Some(true) =>
            {
                Some(
                    value
                        .get("last_id")
                        .and_then(serde_json::Value::as_str)
                        .filter(|cursor| !cursor.is_empty())
                        .ok_or_else(|| {
                            ModelDiscoveryError::InvalidResponse(
                                "Anthropic page has_more=true but carries no last_id".into(),
                            )
                        })?
                        .to_string(),
                )
            }
            AdapterKind::Gemini | AdapterKind::Vertex => value
                .get("nextPageToken")
                .and_then(serde_json::Value::as_str)
                .filter(|token| !token.is_empty())
                .map(str::to_string),
            _ => None,
        };
        let Some(next_cursor) = next_cursor else {
            break;
        };
        if !seen_cursors.insert(next_cursor.clone()) {
            return Err(ModelDiscoveryError::InvalidResponse(
                "provider repeated a pagination cursor".into(),
            ));
        }
        cursor = Some(next_cursor);
    }
    Ok(ids.into_iter().collect())
}

// One model turn can contain a long reasoning prelude followed by a large typed
// tool call. Keep that complete-turn budget independent from the per-event
// silence bound: using the same 120-second value for both made healthy streams
// from reasoning models fail while they were still emitting progress.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// How long the stream may go silent between events before the turn fails as
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
    use super::{DEFAULT_IDLE_TIMEOUT, DEFAULT_TIMEOUT};

    #[test]
    fn complete_reasoning_turn_has_a_larger_budget_than_transport_silence() {
        assert!(DEFAULT_TIMEOUT >= DEFAULT_IDLE_TIMEOUT * 5);
    }
}

fn normalize_provider_base_url(adapter: AdapterKind, base_url: Option<String>) -> Option<String> {
    base_url.map(|base_url| {
        if adapter == AdapterKind::Anthropic {
            // genai concatenates `messages` for this adapter instead of URL-joining
            // it, so a custom path prefix must retain its trailing separator.
            format!("{}/", base_url.trim_end_matches('/'))
        } else {
            base_url
        }
    })
}

/// A `genai::Client` behind the neutral `LlmExecutor` port.
pub struct GenaiExecutor {
    client: Client,
    adapter: Option<AdapterKind>,
    timeout: Duration,
    idle_timeout: Duration,
}

impl GenaiExecutor {
    /// Construct the canonical provider adapter from an existing snapshot
    /// candidate and caller-owned credential material.
    pub fn from_snapshot_candidate(
        candidate: &ResolvedModelCandidate,
        credential: impl Into<String>,
    ) -> std::result::Result<Self, String> {
        let endpoint = match &candidate.provisioning {
            ModelProvisioning::Provider { endpoint, .. } => endpoint,
            _ => {
                return Err(format!(
                    "snapshot candidate `{}` has no provider endpoint",
                    candidate.binding.model_ref
                ));
            }
        };
        let adapter = match endpoint.adapter_kind.trim().to_ascii_lowercase().as_str() {
            "anthropic" => AdapterKind::Anthropic,
            "openai" => AdapterKind::OpenAI,
            "gemini" => AdapterKind::Gemini,
            "vertex" => AdapterKind::Vertex,
            _ => {
                return Err(format!(
                    "unsupported snapshot model adapter `{}`",
                    endpoint.adapter_kind
                ));
            }
        };
        Ok(Self::from_resolved(
            adapter,
            (!endpoint.base_url.trim().is_empty()).then(|| endpoint.base_url.clone()),
            credential,
        ))
    }

    /// Use an explicitly configured client. Product composition must prefer
    /// [`Self::from_resolved`]; this constructor exists for adapters/tests with
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
            timeout: DEFAULT_TIMEOUT,
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
        let base_url = normalize_provider_base_url(adapter, base_url);
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
        Self::with_client_for_adapter(client, adapter)
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
        Self::with_client_for_adapter(client, AdapterKind::Vertex)
    }
}

#[async_trait]
impl LlmExecutor for GenaiExecutor {
    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse> {
        let model = request.model_binding.model_ref.clone();
        let genai_request = to_genai_request_with_adapter(&request, self.adapter)?;
        let options = to_genai_options(&request, false)?;

        let response = tokio::time::timeout(
            self.timeout,
            self.client.exec_chat(model, genai_request, Some(&options)),
        )
        .await
        // A timeout is retryable: the same call may succeed on retry.
        .map_err(|_| Error::Timeout("model call timed out".to_string()))?
        .map_err(|err| classify_error(&err.to_string()))?;

        require_usable_response(from_genai_response(response))
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

        // Have genai assemble the committed turn for us. It concatenates the text
        // chunks and parses the accumulated tool-argument fragments into a JSON
        // object, exactly as the non-streaming path returns them — Anthropic
        // streams tool arguments as `input_json_delta` text that is only valid
        // JSON once the block ends. Without `capture_*` the `End` event carries
        // no content and we'd be stuck with the raw, string-encoded deltas.
        let options = to_genai_options(&request, true)?
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
        // Fallback assembly, used only if the provider delivers no captured turn.
        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        // Per-call bytes already emitted: genai hands CUMULATIVE argument snapshots,
        // so this adapter is the single owner that de-accumulates them into suffix
        // deltas (`ToolCallDelta`); no downstream ever diffs again.
        let mut tool_sent: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let deadline = started + self.timeout;

        loop {
            // Two independent bounds share the one configured call timeout:
            // the fixed deadline catches providers that emit no-op/heartbeat
            // events forever, while the idle window catches a silent connection.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout(
                    "model stream exceeded total timeout".to_string(),
                ));
            }
            let wait = self.idle_timeout.min(remaining);
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
                // Live reasoning: announce progress on the distinct reasoning
                // channel; Managed exposes only a start marker and never the text.
                // The completed form still becomes the committed `Thinking` block.
                ChatStreamEvent::ReasoningChunk(chunk) => {
                    sink.on_reasoning(&chunk.content).await;
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
        // Fold the turn's reasoning through the same canonical response mapper
        // used by non-streaming inference so both paths commit an identical turn.
        let output = with_reasoning(output, Some(&reasoning), ResponseTransport::Streaming);
        require_usable_response(ChatResponse {
            output,
            usage,
            stop_reason,
        })
    }
}

/// Materialize the snapshot's typed inference controls into genai's per-call
/// options. A control the adapter cannot faithfully express is rejected before
/// network I/O; accepting and silently running with another behavior would make
/// the immutable Agent revision untrue.
fn to_genai_options(request: &ChatRequest, _streaming: bool) -> Result<ChatOptions> {
    use awaken_runtime_contract::agent_bindings::{InferenceSpeed, ReasoningEffort};

    if request.inference.speed == Some(InferenceSpeed::Fast) {
        return Err(Error::InvalidRequest(
            "inference speed `fast` is not supported by the configured genai adapter".into(),
        ));
    }
    if let Some(geo) = &request.inference.inference_geo {
        return Err(Error::InvalidRequest(format!(
            "inference_geo `{geo}` is not supported by the configured genai adapter"
        )));
    }
    let mut options = ChatOptions::default();
    if let Some(effort) = request.inference.effort {
        options = options.with_reasoning_effort(match effort {
            ReasoningEffort::Low => GenaiReasoningEffort::Low,
            ReasoningEffort::Medium => GenaiReasoningEffort::Medium,
            ReasoningEffort::High => GenaiReasoningEffort::High,
            ReasoningEffort::Xhigh => GenaiReasoningEffort::XHigh,
            ReasoningEffort::Max => GenaiReasoningEffort::Max,
        });
    }
    Ok(options)
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
        inference: Default::default(),
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
        // turn and some provider protocols reject it. Reasoning that accompanies
        // text or a tool call remains attached and is replayed by adapters that
        // require it (notably DeepSeek's OpenAI-compatible tool loop).
        // The neutral transcript commits one Tool message per completed call.
        // Anthropic Messages instead requires every result for one assistant
        // tool-use turn to appear together in the immediately following user
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
pub fn from_genai_response(response: genai::chat::ChatResponse) -> ChatResponse {
    let output = with_reasoning(
        map_assistant_output(&response.content),
        response.reasoning_content.as_deref(),
        ResponseTransport::NonStreaming,
    );
    ChatResponse {
        output,
        usage: Some(map_usage(&response.usage)),
        stop_reason: response.stop_reason.as_ref().and_then(map_stop_reason),
    }
}

fn require_usable_response(response: ChatResponse) -> Result<ChatResponse> {
    if response.output.text_content().trim().is_empty() && response.output.tool_calls().is_empty() {
        return Err(Error::Provider(
            "model returned no visible assistant content or tool call".into(),
        ));
    }
    // Some OpenAI-compatible reasoning providers occasionally serialize their
    // private tool-call wire language as assistant text. Committing that text as
    // a successful turn silently drops the requested side effect. Do not parse
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
    fn reasoning_only_response_is_retryable_instead_of_committed_as_empty_success() {
        let response = ChatResponse {
            output: AssistantOutput::from_blocks(vec![ContentBlock::thinking("reasoning")]),
            usage: Some(TokenUsage {
                completion_tokens: 7,
                ..TokenUsage::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
        };

        let error = require_usable_response(response).unwrap_err();
        assert!(matches!(error, Error::Provider(_)));
        assert!(error.is_retryable());
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
    #[test]
    fn textual_dsml_tool_calls_always_fail_closed() {
        let serialized = || ChatResponse {
            output: AssistantOutput::text(
                "<｜｜DSML｜｜tool_calls> <｜｜DSML｜｜invoke name=\"publish\">",
            ),
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
        };

        let error = require_usable_response(serialized()).expect_err("T2/E2");
        assert!(matches!(error, Error::Provider(_)), "T2/E2");
        assert!(error.is_retryable(), "T2/E2");

        let ordinary = ChatResponse {
            output: AssistantOutput::text("ordinary final answer"),
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
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

    fn public_turn() -> AssistantOutput {
        AssistantOutput::from_blocks(vec![
            ContentBlock::tool_use("call-1", "read_fixture", serde_json::json!({"id": 1})),
            ContentBlock::text("public answer"),
        ])
    }

    #[test]
    fn streaming_and_non_streaming_commit_the_same_reasoning_shape() {
        let streaming = with_reasoning(
            public_turn(),
            Some("private reasoning"),
            ResponseTransport::Streaming,
        );
        let non_streaming = with_reasoning(
            public_turn(),
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
    use std::time::Duration;

    use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};

    use super::{DEFAULT_IDLE_TIMEOUT, GenaiExecutor, classify_error};

    #[test]
    fn default_stream_idle_window_allows_long_reasoning_prefill() {
        // A live DeepSeek reasoning turn produced no SSE event for just over
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

    /// Cause/effect rules for snapshot adapter construction: R1 an existing
    /// Provider candidate with a supported adapter => executor construction;
    /// R2 HostExecutor/no endpoint => explicit error; R3 unknown adapter => error.
    #[test]
    fn snapshot_candidate_is_the_only_sdk_model_configuration() {
        let provider: ResolvedModelCandidate = serde_json::from_value(serde_json::json!({
            "provider_identity_ref": "local",
            "model_ref": "model-a",
            "backend_ref": "genai",
            "provisioning": {
                "type": "provider",
                "provider_ref": "local",
                "route_ref": "local",
                "scope_id": "local",
                "endpoint": {
                    "adapter_kind": "openai",
                    "api_dialect": "chat_completions",
                    "base_url": "https://gateway.example/v1",
                    "upstream_model": "model-a"
                }
            }
        }))
        .unwrap();
        assert!(GenaiExecutor::from_snapshot_candidate(&provider, "key").is_ok());

        let host = ResolvedModelCandidate::host(provider.binding.clone());
        assert!(
            GenaiExecutor::from_snapshot_candidate(&host, "key")
                .err()
                .unwrap()
                .contains("no provider endpoint")
        );

        let mut unknown = provider;
        if let ModelProvisioning::Provider { endpoint, .. } = &mut unknown.provisioning {
            endpoint.adapter_kind = "unknown".into();
        }
        assert!(
            GenaiExecutor::from_snapshot_candidate(&unknown, "key")
                .err()
                .unwrap()
                .contains("unsupported snapshot model adapter")
        );
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
    use awaken_runtime_contract::agent_bindings::{
        InferenceOptions, InferenceSpeed, ReasoningEffort,
    };
    use awaken_runtime_contract::llm::{
        ChatMessage, ChatRequest, DeltaSink, LlmExecutor, StopReason,
    };
    use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        AdapterKind, CredentialProbe, GenaiExecutor, GenaiReasoningEffort, discover_model_ids,
        normalize_provider_base_url, probe_credential, to_genai_options,
    };

    #[test]
    fn anthropic_gateway_base_preserves_its_path_separator() {
        assert_eq!(
            normalize_provider_base_url(
                AdapterKind::Anthropic,
                Some("https://api.deepseek.com/anthropic".into()),
            )
            .as_deref(),
            Some("https://api.deepseek.com/anthropic/")
        );
        assert_eq!(
            normalize_provider_base_url(
                AdapterKind::OpenAI,
                Some("https://api.deepseek.com".into()),
            )
            .as_deref(),
            Some("https://api.deepseek.com")
        );
    }

    fn controlled_request(inference: InferenceOptions) -> ChatRequest {
        ChatRequest {
            model_binding: ModelBinding::new("provider", "claude-opus-4-8", "genai"),
            inference,
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::text("hello")],
            }],
            tools: Vec::new(),
        }
    }

    #[test]
    fn typed_inference_controls_are_materialized_or_rejected_before_io() {
        // Causal graph:
        // immutable ChatRequest controls -> provider adapter options OR stable
        // pre-I/O rejection. No branch may silently discard a requested mode.
        //
        // Decision table:
        // | effort | speed    | geo  | provider behavior                         |
        // | high   | standard | none | genai reasoning_effort=High              |
        // | none   | omitted  | none | default options                          |
        // | any    | fast     | none | invalid_request before network execution |
        // | any    | any      | set  | invalid_request before network execution |
        let standard = controlled_request(InferenceOptions {
            effort: Some(ReasoningEffort::High),
            speed: Some(InferenceSpeed::Standard),
            inference_geo: None,
        });
        let options = to_genai_options(&standard, false).unwrap();
        assert!(matches!(
            options.reasoning_effort,
            Some(GenaiReasoningEffort::High)
        ));

        let defaults = to_genai_options(&controlled_request(Default::default()), false).unwrap();
        assert!(defaults.reasoning_effort.is_none());

        let fast = controlled_request(InferenceOptions {
            effort: Some(ReasoningEffort::Max),
            speed: Some(InferenceSpeed::Fast),
            inference_geo: None,
        });
        let error = to_genai_options(&fast, false).unwrap_err();
        assert_eq!(error.code(), "invalid_request");

        let geo = controlled_request(InferenceOptions {
            effort: None,
            speed: None,
            inference_geo: Some(awaken_runtime_contract::agent_bindings::InferenceGeography::Us),
        });
        let error = to_genai_options(&geo, false).unwrap_err();
        assert_eq!(error.code(), "invalid_request");
        assert!(error.to_string().contains("inference_geo `us`"));
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
        let models = discover_model_ids(AdapterKind::Anthropic, Some(&base), "private-key")
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
        let models = discover_model_ids(AdapterKind::Gemini, Some(&base), "private-key")
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
        let error = discover_model_ids(AdapterKind::Anthropic, Some(&base), "private-key")
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
