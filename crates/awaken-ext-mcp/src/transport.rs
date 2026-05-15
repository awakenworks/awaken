//! MCP tool transport: wraps MCP tool calls as awaken `Tool` implementations.
//!
//! Contains the `McpToolTransport` trait (raw MCP client abstraction) and
//! `McpTool` which adapts an MCP tool definition into an awaken `Tool`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures::StreamExt;
use mcp::transport::{
    ClientInfo, InitializeCapabilities, InitializeResult, McpServerConnectionConfig,
    McpTransportError, SamplingCapabilities, ServerCapabilities, TransportTypeId,
};
use mcp::{
    CallToolParams, CallToolResult, CompleteParams, CompleteResult, CreateMessageParams, JsonRpcId,
    JsonRpcMessage, JsonRpcNotification, JsonRpcPayload, JsonRpcRequest, JsonRpcResponse,
    ListRootsResult, ListToolsResult, MCP_PROTOCOL_VERSION, McpToolDefinition,
    ProgressNotificationParams, ProgressToken, Root, ToolContent,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use awaken_contract::cancellation::CancellationToken;

use crate::progress::McpProgressUpdate;
use crate::sampling::SamplingHandler;

/// Sentinel error string used to distinguish a client-initiated
/// cancellation from other transport errors at the call boundary. Kept
/// as a string so the variant set in the upstream `McpTransportError`
/// crate doesn't need extending — callers match on this exact message
/// to surface the cancellation upward (e.g. as `ToolError::Cancelled`).
pub const CANCELLED_BY_CLIENT: &str = "MCP request cancelled by client";

const MCP_SESSION_EXPIRED: &str = "MCP session expired";
const MCP_SESSION_EXPIRED_AFTER_ACCEPT: &str = "MCP session expired after request was accepted";
const MAX_SSE_LINE_BYTES: usize = 64 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
const MAX_SSE_JSON_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_SSE_RETRY_DELAY_MS: u64 = 30_000;
const MAX_SSE_RESUME_ATTEMPTS: u32 = 3;
pub(crate) const LIST_CHANGED_CHANNEL_CAPACITY: usize = 128;
pub(crate) const MCP_PROGRESS_CHANNEL_CAPACITY: usize = 128;
pub(crate) const RESOURCE_UPDATED_INGRESS_CAPACITY: usize = 1024;

/// Protocol versions awaken's transports know how to talk on the wire.
///
/// Per MCP 2025-11-25 §Lifecycle / Version Negotiation: "If the server
/// supports the requested protocol version, it MUST respond with the
/// same version. Otherwise, the server MUST respond with another
/// protocol version it supports." Our handshake sends
/// [`MCP_PROTOCOL_VERSION`] (the version baked into the upstream `mcp`
/// crate); we accept the server's response only if it appears in this
/// list. Mismatch is a hard error rather than a silent downgrade —
/// silent downgrade would mean we send wire shapes the server can't
/// parse, with failures surfacing far from the cause.
///
/// Order is purely informational. The list intentionally stays narrow
/// (one entry today); widen it only when wire-format compatibility for
/// an older spec rev has been verified end-to-end.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[MCP_PROTOCOL_VERSION];

/// Validate the `protocolVersion` echoed by the server in `InitializeResult`.
///
/// Returns the negotiated version (always one of [`SUPPORTED_PROTOCOL_VERSIONS`])
/// or a [`McpTransportError::ProtocolError`] describing the mismatch.
/// The error names both sides so operators don't have to dig through
/// logs to see which way to upgrade.
pub(crate) fn negotiate_protocol_version(
    server_version: &str,
) -> Result<&'static str, McpTransportError> {
    if let Some(accepted) = SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .find(|v| **v == server_version)
    {
        Ok(*accepted)
    } else {
        Err(McpTransportError::ProtocolError(format!(
            "MCP server replied with unsupported protocolVersion {server_version:?}; \
             awaken supports {SUPPORTED_PROTOCOL_VERSIONS:?}. The server should respond \
             with one of these or upgrade to {MCP_PROTOCOL_VERSION}."
        )))
    }
}

#[cfg(unix)]
use nix::sys::signal::{Signal, kill};
#[cfg(unix)]
use nix::unistd::Pid;

type PendingRequestSender = oneshot::Sender<Result<Value, McpTransportError>>;
type PendingRequests = Arc<tokio::sync::Mutex<HashMap<i64, PendingRequestSender>>>;

// ── Prompt/Resource types ──

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<McpPromptArgument>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptMessage {
    pub role: String,
    pub content: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub messages: Vec<McpPromptMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpResourceDefinition {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct ListPromptsResult {
    #[serde(default)]
    prompts: Vec<McpPromptDefinition>,
}

#[derive(Debug, Clone, Deserialize)]
struct ListResourcesResult {
    #[serde(default)]
    resources: Vec<McpResourceDefinition>,
}

// ── McpCallMetadata ──

/// Client-side attribution metadata attached to outgoing MCP tool calls
/// via JSON-RPC `params._meta`. Lets the MCP server identify which agent /
/// thread / run / call initiated the request so it can do per-agent rate
/// limiting, per-tenant OAuth, audit, or workflow correlation.
///
/// Spec (2025-11-25 §JSON-RPC 2.0 + §Basic) reserves the `_meta` field on
/// request params for client-controlled metadata. By convention,
/// vendor-specific keys are namespaced — we use `awaken/attribution` so
/// our additions don't collide with future MCP spec fields (notably the
/// existing `progressToken` key, which we continue to set in the same
/// `_meta` map).
///
/// All fields are optional. Empty `McpCallMetadata` is a no-op — no
/// `awaken/attribution` key is added to `_meta`.
#[derive(Debug, Clone, Default)]
pub struct McpCallMetadata {
    pub agent_id: Option<String>,
    pub thread_id: Option<String>,
    pub run_id: Option<String>,
    pub call_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub parent_call_id: Option<String>,
}

impl McpCallMetadata {
    /// Serialize set fields into a `Map` under the `awaken/attribution`
    /// key. No-op if every field is `None`.
    fn write_into(&self, map: &mut Map<String, Value>) {
        let mut bag = Map::new();
        if let Some(v) = &self.agent_id {
            bag.insert("agent_id".to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.thread_id {
            bag.insert("thread_id".to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.run_id {
            bag.insert("run_id".to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.call_id {
            bag.insert("call_id".to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.parent_run_id {
            bag.insert("parent_run_id".to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.parent_call_id {
            bag.insert("parent_call_id".to_string(), Value::String(v.clone()));
        }
        if !bag.is_empty() {
            map.insert("awaken/attribution".to_string(), Value::Object(bag));
        }
    }
}

/// Per-call bundle threading agent / thread / run identity, cancellation,
/// and a sampling handler down to the MCP transport for a single
/// `call_tool` invocation. Previously these were three separate
/// parameters — combined here so adding a new dimension (logging
/// override, deadline, etc.) doesn't churn every trait impl.
///
/// `Default` produces an empty context: no attribution, no cancellation,
/// no per-call sampling handler. Transport behaviour then collapses to
/// the legacy "registry-level fixed handler" path.
#[derive(Default)]
pub struct McpCallContext {
    /// Vendor attribution surfaced to the server via `params._meta.awaken/attribution`.
    pub metadata: McpCallMetadata,
    /// Caller-supplied cancellation token. When fired during an in-flight
    /// `tools/call`, the transport emits `notifications/cancelled` and
    /// returns the [`CANCELLED_BY_CLIENT`] sentinel error.
    pub cancellation: Option<CancellationToken>,
    /// Decision about how server-initiated `sampling/createMessage`
    /// during this call should be routed. See [`McpCallSampling`].
    pub sampling: McpCallSampling,
}

/// Per-call sampling routing decision. Three explicit states so the
/// transport can distinguish "no factory configured at all" from
/// "factory consulted but declined to bind this agent" — these have
/// different security semantics.
#[derive(Clone, Default)]
pub enum McpCallSampling {
    /// No per-call decision was made. The transport falls through to
    /// its registry-level fixed handler (legacy behaviour, preserved
    /// for callers that don't wire a factory).
    #[default]
    Inherit,
    /// Factory bound a specific handler to this call. Server-initiated
    /// `sampling/createMessage` for this call's id routes here, not to
    /// the transport's fallback. Mandatory for multi-agent correctness.
    Bound(Arc<dyn SamplingHandler>),
    /// Factory was consulted and explicitly refused to bind a handler
    /// (e.g. agent's model_id doesn't resolve, agent opted out, tenant
    /// has no sampling quota). The transport MUST reject
    /// `sampling/createMessage` for this call with method-not-supported
    /// — falling through to a global fallback would re-introduce the
    /// cross-agent leak the factory exists to prevent.
    Denied,
}

/// Internal map value mirroring [`McpCallSampling`] minus the `Inherit`
/// variant (Inherit is represented by the absence of a map entry).
#[derive(Clone)]
enum PerCallSamplingEntry {
    Bound(Arc<dyn SamplingHandler>),
    Denied,
}

/// Build the `_meta` value for `tools/call` params. Combines the MCP
/// `progressToken` (when progress is enabled) with optional vendor
/// attribution from `McpCallMetadata`. Returns `None` when neither is
/// present so the wire payload omits the `_meta` field entirely.
fn build_call_tool_meta(
    progress_token: Option<ProgressToken>,
    metadata: &McpCallMetadata,
) -> Result<Option<Value>, McpTransportError> {
    let mut map = Map::new();
    if let Some(token) = progress_token {
        map.insert("progressToken".to_string(), serde_json::to_value(token)?);
    }
    metadata.write_into(&mut map);
    if map.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::Object(map)))
    }
}

// ── McpToolTransport trait ──

/// Raw MCP client transport abstraction.
///
/// Implementations handle the wire protocol (stdio, HTTP) and expose
/// MCP operations as async methods.
#[async_trait]
pub trait McpToolTransport: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError>;

    async fn server_capabilities(&self) -> Result<Option<ServerCapabilities>, McpTransportError> {
        Ok(None)
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
        Err(McpTransportError::TransportError(
            "list_prompts not supported".to_string(),
        ))
    }

    async fn get_prompt(
        &self,
        _name: &str,
        _arguments: Option<HashMap<String, String>>,
    ) -> Result<McpPromptResult, McpTransportError> {
        Err(McpTransportError::TransportError(
            "get_prompt not supported".to_string(),
        ))
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
        Err(McpTransportError::TransportError(
            "list_resources not supported".to_string(),
        ))
    }

    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
        context: McpCallContext,
    ) -> Result<CallToolResult, McpTransportError>;

    fn transport_type(&self) -> TransportTypeId;

    async fn read_resource(&self, _uri: &str) -> Result<Value, McpTransportError> {
        Err(McpTransportError::TransportError(
            "read_resource not supported".to_string(),
        ))
    }

    async fn close(&self) -> Result<(), McpTransportError> {
        Ok(())
    }

    /// Wall-clock time of the current HTTP session's successful
    /// `initialize`, if the transport can report it live. Stdio returns
    /// `None`; manager snapshots fall back to their connect/reconnect cache.
    async fn current_session_started_at(&self) -> Option<SystemTime> {
        None
    }

    /// HTTP session generation, if the transport has session state.
    /// Streamable HTTP increments this counter on local session reset
    /// and reinitialize cycles, including 404 session expiry. Stdio has
    /// no session concept and returns `None`.
    async fn current_session_generation(&self) -> Option<u64> {
        None
    }

    /// Take the receiver for queued `notifications/tools/list_changed`
    /// events. Per MCP 2025-11-25 §Server features, servers that
    /// advertise `tools.listChanged` SHOULD emit this notification when
    /// the tool catalogue mutates — receiving one removes the need for
    /// periodic polling for that cached surface. Prompt/resource
    /// list_changed notifications are parsed but not queued because
    /// those catalogues are fetched live today.
    ///
    /// One-shot: callable at most once per transport instance. Returns
    /// `None` thereafter (and `None` for transports that don't emit
    /// these notifications, e.g. test stubs).
    async fn take_list_changed_receiver(&self) -> Option<mpsc::Receiver<ListChangedKind>> {
        None
    }

    /// Subscribe to updates for a single resource URI. Per MCP
    /// 2025-11-25 §Resources / Subscriptions: callable only when the
    /// server advertised `resources.subscribe: true` during initialize.
    /// The caller is responsible for that capability check; the
    /// transport just forwards the JSON-RPC request.
    async fn subscribe_resource(&self, _uri: &str) -> Result<(), McpTransportError> {
        Err(McpTransportError::TransportError(
            "subscribe_resource not supported".to_string(),
        ))
    }

    /// Cancel a prior subscription. Mirrors [`subscribe_resource`] —
    /// caller is responsible for capability gating.
    async fn unsubscribe_resource(&self, _uri: &str) -> Result<(), McpTransportError> {
        Err(McpTransportError::TransportError(
            "unsubscribe_resource not supported".to_string(),
        ))
    }

    /// Take the receiver for `notifications/resources/updated` events.
    /// Each event carries the URI of the updated resource. The host
    /// can fetch the new contents via `read_resource` in response.
    /// One-shot like [`take_list_changed_receiver`].
    async fn take_resource_updated_receiver(&self) -> Option<mpsc::Receiver<String>> {
        None
    }

    /// Ask the server for autocomplete suggestions for a prompt /
    /// resource argument. Per MCP 2025-11-25 §Utilities / Completion:
    /// only valid when the server advertised `completions: {}` in its
    /// initialize response — the caller is responsible for that
    /// capability check. The transport just forwards the request.
    async fn complete(&self, _params: CompleteParams) -> Result<CompleteResult, McpTransportError> {
        Err(McpTransportError::TransportError(
            "completion/complete not supported".to_string(),
        ))
    }
}

/// Which server-side catalogue the `notifications/.../list_changed`
/// event refers to.
///
/// The manager currently caches only the tools catalogue, so transports
/// forward `Tools` into the bounded refresh queue and ignore
/// `Prompts`/`Resources` after parsing them. Prompt and resource lists
/// are fetched live through the manager API today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListChangedKind {
    /// `notifications/tools/list_changed` — re-run `tools/list`.
    Tools,
    /// `notifications/prompts/list_changed` — parsed but not queued today.
    Prompts,
    /// `notifications/resources/list_changed` — parsed but not queued today.
    Resources,
}

/// Parse a JSON-RPC method name into a recognised list-changed kind.
/// Returns `None` for any method that isn't one of the three
/// standard `notifications/.../list_changed` strings.
fn list_changed_method(method: &str) -> Option<ListChangedKind> {
    match method {
        "notifications/tools/list_changed" => Some(ListChangedKind::Tools),
        "notifications/prompts/list_changed" => Some(ListChangedKind::Prompts),
        "notifications/resources/list_changed" => Some(ListChangedKind::Resources),
        _ => None,
    }
}

fn forward_list_changed(sender: &mpsc::Sender<ListChangedKind>, kind: ListChangedKind) {
    match kind {
        ListChangedKind::Tools => match sender.try_send(kind) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!(
                    capacity = LIST_CHANGED_CHANNEL_CAPACITY,
                    "coalescing MCP tools/list_changed notification because refresh queue is full"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!("dropping MCP tools/list_changed notification; receiver is closed");
            }
        },
        ListChangedKind::Prompts | ListChangedKind::Resources => {
            tracing::debug!(
                kind = ?kind,
                "ignoring MCP list_changed notification for uncached catalogue"
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SseEventIdUpdate {
    Absent,
    Set(String),
    Reset,
}

/// Extract the value of an `id:` field from the buffered SSE event lines.
/// Per WHATWG SSE, an empty `id:` resets the last-event-id; that must be
/// represented separately from an absent field so callers can clear stale
/// resume cursors.
fn extract_event_id(event_lines: &[String]) -> SseEventIdUpdate {
    for line in event_lines.iter().rev() {
        if let Some(rest) = line.strip_prefix("id:") {
            let trimmed = rest.trim_start();
            if trimmed.is_empty() {
                return SseEventIdUpdate::Reset;
            }
            return SseEventIdUpdate::Set(trimmed.to_string());
        }
    }
    SseEventIdUpdate::Absent
}

fn apply_event_id_update(last_event_id: &mut Option<String>, update: SseEventIdUpdate) {
    match update {
        SseEventIdUpdate::Absent => {}
        SseEventIdUpdate::Set(id) => *last_event_id = Some(id),
        SseEventIdUpdate::Reset => *last_event_id = None,
    }
}

/// Extract an SSE `retry:` field as a reconnect delay. Malformed values are
/// ignored, matching the EventSource processing model.
fn extract_retry_delay(event_lines: &[String]) -> Option<Duration> {
    let mut delay = None;
    for line in event_lines {
        if let Some(rest) = line.strip_prefix("retry:") {
            let trimmed = rest.trim_start();
            if let Ok(millis) = trimmed.parse::<u64>() {
                delay = Some(Duration::from_millis(millis));
            }
        }
    }
    delay
}

fn bounded_sse_retry_delay(
    event_lines: &[String],
    context: &str,
) -> Result<Option<Duration>, McpTransportError> {
    let Some(delay) = extract_retry_delay(event_lines) else {
        return Ok(None);
    };
    if delay > Duration::from_millis(MAX_SSE_RETRY_DELAY_MS) {
        return Err(McpTransportError::ProtocolError(format!(
            "{context} SSE retry delay exceeded {MAX_SSE_RETRY_DELAY_MS} ms"
        )));
    }
    Ok(Some(delay))
}

fn response_content_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn is_sse_content_type(content_type: &str) -> bool {
    content_type.starts_with("text/event-stream")
}

fn validate_sse_response(
    response: &reqwest::Response,
    context: &str,
) -> Result<(), McpTransportError> {
    let content_type = response_content_type(response);
    if is_sse_content_type(&content_type) {
        Ok(())
    } else {
        Err(McpTransportError::ProtocolError(format!(
            "{context} expected Content-Type text/event-stream, got {}",
            if content_type.is_empty() {
                "<missing>"
            } else {
                content_type.as_str()
            }
        )))
    }
}

async fn decode_json_response_body(
    response: reqwest::Response,
    timeout: Duration,
) -> Result<Value, McpTransportError> {
    tokio::time::timeout(timeout, response.json())
        .await
        .map_err(|_| {
            McpTransportError::Timeout(format!(
                "HTTP JSON response body did not complete within {:?}",
                timeout
            ))
        })?
        .map_err(|e| {
            McpTransportError::TransportError(format!("Failed to parse JSON response: {}", e))
        })
}

fn push_sse_line_byte(
    line_buf: &mut Vec<u8>,
    byte: u8,
    context: &str,
) -> Result<(), McpTransportError> {
    if line_buf.len() >= MAX_SSE_LINE_BYTES {
        return Err(McpTransportError::ProtocolError(format!(
            "{context} SSE line exceeded {MAX_SSE_LINE_BYTES} bytes"
        )));
    }
    line_buf.push(byte);
    Ok(())
}

fn push_sse_event_line(
    event_lines: &mut Vec<String>,
    event_bytes: &mut usize,
    line: String,
    context: &str,
) -> Result<(), McpTransportError> {
    *event_bytes = event_bytes
        .checked_add(line.len().saturating_add(1))
        .ok_or_else(|| {
            McpTransportError::ProtocolError(format!(
                "{context} SSE event exceeded {MAX_SSE_EVENT_BYTES} bytes"
            ))
        })?;
    if *event_bytes > MAX_SSE_EVENT_BYTES {
        return Err(McpTransportError::ProtocolError(format!(
            "{context} SSE event exceeded {MAX_SSE_EVENT_BYTES} bytes"
        )));
    }
    event_lines.push(line);
    Ok(())
}

fn sse_data_payload(
    event_lines: &[String],
    context: &str,
) -> Result<Option<String>, McpTransportError> {
    let mut data_parts = Vec::new();
    let mut payload_bytes = 0usize;
    for line in event_lines {
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let part = rest.trim_start();
            payload_bytes = payload_bytes
                .checked_add(part.len())
                .and_then(|bytes| bytes.checked_add(1))
                .ok_or_else(|| {
                    McpTransportError::ProtocolError(format!(
                        "{context} SSE JSON payload exceeded {MAX_SSE_JSON_PAYLOAD_BYTES} bytes"
                    ))
                })?;
            if payload_bytes > MAX_SSE_JSON_PAYLOAD_BYTES {
                return Err(McpTransportError::ProtocolError(format!(
                    "{context} SSE JSON payload exceeded {MAX_SSE_JSON_PAYLOAD_BYTES} bytes"
                )));
            }
            data_parts.push(part.to_string());
        }
    }

    if data_parts.is_empty() {
        return Ok(None);
    }
    let payload = data_parts.join("\n");
    if payload.is_empty() {
        Ok(None)
    } else {
        Ok(Some(payload))
    }
}

/// Parse a `notifications/resources/updated` notification's params into
/// the affected resource URI. Returns `None` if the method doesn't
/// match or the params don't carry a string `uri`.
fn resource_updated_uri(notification: &JsonRpcNotification) -> Option<String> {
    if notification.method != "notifications/resources/updated" {
        return None;
    }
    notification
        .params
        .as_ref()?
        .get("uri")?
        .as_str()
        .map(str::to_string)
}

// ── Progress token key ──

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub(crate) enum ProgressTokenKey {
    String(String),
    Number(i64),
}

impl From<&ProgressToken> for ProgressTokenKey {
    fn from(token: &ProgressToken) -> Self {
        match token {
            ProgressToken::String(v) => ProgressTokenKey::String(v.clone()),
            ProgressToken::Number(v) => ProgressTokenKey::Number(*v),
        }
    }
}

// ── Write request ──

struct WriteRequest {
    line: String,
    /// Optional ack channel: when present, the writer task signals after
    /// the line has been written + flushed to the subprocess stdin. Used
    /// by the cancellation path so `notifications/cancelled` is
    /// guaranteed to reach the subprocess before the transport is
    /// dropped (drop kills the subprocess via `kill_on_drop(true)`).
    ack: Option<oneshot::Sender<()>>,
}

/// Type alias for the per-call sampling-handler map shared between
/// `call_tool` (which inserts on entry, removes on exit) and the
/// background reader/dispatcher (which looks up by in-flight call id when
/// handling server-initiated `sampling/createMessage`).
type PerCallSamplingHandlers = Arc<tokio::sync::Mutex<HashMap<i64, PerCallSamplingEntry>>>;

/// RAII guard that inserts a per-call sampling entry at construction
/// and removes it on drop. Registration is async/deterministic — the
/// call_tool path awaits the lock so the entry is guaranteed visible
/// before the request is sent on the wire. Without this guarantee the
/// reader could observe a sampling/createMessage for our call before
/// the entry exists and route to a stale fallback handler.
struct PerCallSamplingGuard {
    handlers: PerCallSamplingHandlers,
    id: i64,
    /// `false` when the call passed `McpCallSampling::Inherit` — no
    /// entry was registered, so drop has nothing to remove.
    active: bool,
}

impl PerCallSamplingGuard {
    /// Register the per-call sampling decision for `id`. Awaits the
    /// map lock — never silently skips registration. Callers MUST await
    /// this before sending the request id on the wire, otherwise the
    /// reader can race a server-initiated `sampling/createMessage` for
    /// the call and miss the entry.
    async fn register(
        handlers: PerCallSamplingHandlers,
        id: i64,
        sampling: McpCallSampling,
    ) -> Self {
        match sampling {
            McpCallSampling::Inherit => Self {
                handlers,
                id,
                active: false,
            },
            McpCallSampling::Bound(h) => {
                handlers
                    .lock()
                    .await
                    .insert(id, PerCallSamplingEntry::Bound(h));
                Self {
                    handlers,
                    id,
                    active: true,
                }
            }
            McpCallSampling::Denied => {
                handlers
                    .lock()
                    .await
                    .insert(id, PerCallSamplingEntry::Denied);
                Self {
                    handlers,
                    id,
                    active: true,
                }
            }
        }
    }

    /// Explicitly drop the per-call entry with an `.await`'d lock.
    /// Call this on every exit path of `call_tool` so request-bound
    /// routing does not observe a stale entry from a call that just
    /// returned. Marks the guard inactive so the sync `Drop` impl
    /// becomes a no-op safety net.
    async fn unregister(&mut self) {
        if !self.active {
            return;
        }
        self.handlers.lock().await.remove(&self.id);
        self.active = false;
    }
}

impl Drop for PerCallSamplingGuard {
    fn drop(&mut self) {
        // Reachable only if `unregister()` wasn't awaited (panic,
        // early return on a path the future of call_tool aborts mid-
        // way). Sync best-effort: try_lock first, spawn fallback for
        // contended cases. The contended path is racy by design — we
        // accept it as a backstop ONLY; correctness for the routing
        // path comes from `unregister()` being called before return.
        if !self.active {
            return;
        }
        if let Ok(mut map) = self.handlers.try_lock() {
            map.remove(&self.id);
        } else {
            let handlers = Arc::clone(&self.handlers);
            let id = self.id;
            tokio::task::spawn(async move {
                handlers.lock().await.remove(&id);
            });
        }
    }
}

// ── Stdio transport ──

pub(crate) struct ProgressAwareStdioTransport {
    write_tx: mpsc::Sender<WriteRequest>,
    pending: PendingRequests,
    progress_subscribers:
        Arc<tokio::sync::Mutex<HashMap<ProgressTokenKey, mpsc::Sender<McpProgressUpdate>>>>,
    next_id: AtomicI64,
    next_progress_token: AtomicI64,
    alive: Arc<AtomicBool>,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
    timeout: Duration,
    capabilities: Option<ServerCapabilities>,
    /// Map of in-flight tool-call JSON-RPC ids → per-call sampling
    /// decision (Bound | Denied). HTTP request-bound SSE can correlate
    /// server requests back to one client request; stdio cannot. The
    /// stdio reader therefore never consults this map for server-
    /// initiated sampling, but the call guard still owns cleanup for
    /// shared helper code and future transports.
    per_call_sampling: PerCallSamplingHandlers,
    /// Receiver half of the `notifications/.../list_changed` channel.
    /// The reader task owns the cloned sender; when this transport (and
    /// thus the reader task) is dropped, the channel closes naturally.
    /// Taken at most once by [`take_list_changed_receiver`].
    list_changed_rx: tokio::sync::Mutex<Option<mpsc::Receiver<ListChangedKind>>>,
    /// Receiver half of the `notifications/resources/updated` channel.
    /// Each event is the URI of an updated resource. Mirrors the
    /// list_changed pattern — one-shot, taken via
    /// [`take_resource_updated_receiver`].
    resource_updated_rx: tokio::sync::Mutex<Option<mpsc::Receiver<String>>>,
    /// Client-defined roots advertised to the server during `initialize`
    /// and served on subsequent `roots/list` requests. Empty = roots
    /// capability not advertised. Held as an `Arc` so the spawned
    /// reader task can share it cheaply with the initialize path.
    roots: Arc<Vec<Root>>,
}

impl ProgressAwareStdioTransport {
    pub(crate) async fn connect(
        config: &McpServerConnectionConfig,
        sampling_handler: Option<Arc<dyn SamplingHandler>>,
        roots: Arc<Vec<Root>>,
        advertise_sampling: bool,
    ) -> Result<Self, McpTransportError> {
        let command = config.command.as_ref().ok_or_else(|| {
            McpTransportError::TransportError("Stdio transport requires command".to_string())
        })?;

        let mut cmd = Command::new(command);
        cmd.args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().map_err(|e| {
            McpTransportError::TransportError(format!(
                "Failed to spawn process '{}': {}",
                command, e
            ))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpTransportError::TransportError("Failed to get stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpTransportError::TransportError("Failed to get stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| McpTransportError::TransportError("Failed to get stderr".to_string()))?;

        let alive = Arc::new(AtomicBool::new(true));
        let pending: PendingRequests = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let progress_subscribers: Arc<
            tokio::sync::Mutex<HashMap<ProgressTokenKey, mpsc::Sender<McpProgressUpdate>>>,
        > = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let (write_tx, mut write_rx) = mpsc::channel::<WriteRequest>(256);
        let alive_writer = Arc::clone(&alive);
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(req) = write_rx.recv().await {
                if !alive_writer.load(Ordering::SeqCst) {
                    break;
                }
                if let Err(e) = stdin.write_all(req.line.as_bytes()).await {
                    tracing::error!(error = %e, "MCP stdio write error");
                    alive_writer.store(false, Ordering::SeqCst);
                    break;
                }
                if let Err(e) = stdin.flush().await {
                    tracing::error!(error = %e, "MCP stdio flush error");
                    alive_writer.store(false, Ordering::SeqCst);
                    break;
                }
                if let Some(ack) = req.ack {
                    let _ = ack.send(());
                }
            }
        });

        let pending_reader = Arc::clone(&pending);
        let progress_reader = Arc::clone(&progress_subscribers);
        let alive_reader = Arc::clone(&alive);
        let write_tx_reader = write_tx.clone();
        let sampling_handler_reader = sampling_handler.clone();
        let per_call_sampling: PerCallSamplingHandlers =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let (list_changed_tx, list_changed_rx) =
            mpsc::channel::<ListChangedKind>(LIST_CHANGED_CHANNEL_CAPACITY);
        let list_changed_tx_reader = list_changed_tx.clone();
        let (resource_updated_tx, resource_updated_rx) =
            mpsc::channel::<String>(RESOURCE_UPDATED_INGRESS_CAPACITY);
        let resource_updated_tx_reader = resource_updated_tx.clone();
        let roots_reader = Arc::clone(&roots);
        let mut reader = BufReader::new(stdout);
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        alive_reader.store(false, Ordering::SeqCst);
                        break;
                    }
                    Ok(_) => match serde_json::from_str::<JsonRpcMessage>(&line) {
                        Ok(JsonRpcMessage::Response(response)) => {
                            if let JsonRpcId::Number(id) = response.id {
                                let tx = pending_reader.lock().await.remove(&id);
                                if let Some(tx) = tx {
                                    let result = map_response_payload(response.payload);
                                    let _ = tx.send(result);
                                }
                            }
                        }
                        Ok(JsonRpcMessage::Notification(notification)) => {
                            if let Some(kind) = list_changed_method(&notification.method) {
                                // Best-effort: if the host hasn't taken
                                // the receiver yet (or has dropped it),
                                // a send error just means the host is
                                // not interested — drop silently.
                                forward_list_changed(&list_changed_tx_reader, kind);
                            } else if let Some(uri) = resource_updated_uri(&notification) {
                                let _ = resource_updated_tx_reader.try_send(uri);
                            } else {
                                handle_progress_notification(&progress_reader, notification).await;
                            }
                        }
                        Ok(JsonRpcMessage::Request(request)) => {
                            let fallback = sampling_handler_reader.clone();
                            let wtx = write_tx_reader.clone();
                            let roots = Arc::clone(&roots_reader);
                            tokio::spawn(async move {
                                let chosen = select_stdio_sampling_handler(fallback.as_ref());
                                let response = handle_server_request(
                                    chosen.as_deref(),
                                    roots.as_slice(),
                                    &request,
                                )
                                .await;
                                let line = format!(
                                    "{}\n",
                                    serde_json::to_string(&response).unwrap_or_default()
                                );
                                let _ = wtx.send(WriteRequest { line, ack: None }).await;
                            });
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                message = %line.trim(),
                                "Failed to parse MCP message from stdio"
                            );
                        }
                    },
                    Err(e) => {
                        tracing::error!(error = %e, "MCP stdio read error");
                        alive_reader.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }

            {
                let mut pending = pending_reader.lock().await;
                for (_, tx) in pending.drain() {
                    let _ = tx.send(Err(McpTransportError::ConnectionClosed));
                }
            }
            progress_reader.lock().await.clear();
        });

        tokio::spawn(async move {
            let mut stderr_reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match stderr_reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => tracing::debug!(message = %line.trim_end(), "MCP stdio stderr"),
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to drain MCP stdio stderr");
                        break;
                    }
                }
            }
        });

        // Drop the originating senders so the only live senders are the
        // reader task's clones — when the reader exits (child stdout
        // EOF), the channels naturally close and the host consumer
        // tasks wake up.
        drop(list_changed_tx);
        drop(resource_updated_tx);
        let transport = Self {
            write_tx,
            pending,
            progress_subscribers,
            next_id: AtomicI64::new(1),
            next_progress_token: AtomicI64::new(1),
            alive,
            child: Arc::new(tokio::sync::Mutex::new(Some(child))),
            timeout: Duration::from_secs(config.timeout_secs),
            capabilities: None,
            per_call_sampling,
            list_changed_rx: tokio::sync::Mutex::new(Some(list_changed_rx)),
            resource_updated_rx: tokio::sync::Mutex::new(Some(resource_updated_rx)),
            roots: Arc::clone(&roots),
        };

        let mut capabilities = InitializeCapabilities::default();
        // Advertise `sampling` only when this transport has a handler
        // that can safely answer global server-initiated requests. A
        // per-call factory is not global capability support; it is only
        // safe when the stream binds the request to a specific call id.
        if sampling_handler.is_some() || advertise_sampling {
            capabilities.sampling = Some(SamplingCapabilities::default());
        }
        if !transport.roots.is_empty() {
            // Spec 2025-11-25 §Client features: clients advertising
            // `roots` MUST be prepared to respond to `roots/list`. Our
            // roots are static post-construction (set via the
            // constructor arg, no runtime mutation today), so we
            // advertise `listChanged: false`.
            capabilities.roots = Some(mcp::transport::RootsCapabilities {
                list_changed: Some(false),
            });
        }
        let init_result = match transport
            .send_request(
                "initialize",
                Some(initialize_params(
                    serde_json::to_value(&capabilities).unwrap_or_else(|_| json!({})),
                    config.config.clone(),
                )),
                None,
            )
            .await
        {
            Ok(value) => serde_json::from_value::<InitializeResult>(value)?,
            Err(err) => {
                let _ = transport.close().await;
                return Err(err);
            }
        };
        // Validate the server-echoed protocolVersion BEFORE emitting
        // `notifications/initialized`. A mismatch means the rest of the
        // session would speak wire shapes the server can't parse —
        // better to fail fast at handshake than have every subsequent
        // tools/call surface a confusing parse error.
        if let Err(err) = negotiate_protocol_version(&init_result.protocol_version) {
            let _ = transport.close().await;
            return Err(err);
        }
        let _ = transport
            .send_notification("notifications/initialized", Some(json!({})))
            .await;

        Ok(Self {
            capabilities: Some(init_result.capabilities),
            ..transport
        })
    }

    async fn send_notification(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), McpTransportError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }
        let notification = JsonRpcNotification::new(method, params);
        let line = format!("{}\n", serde_json::to_string(&notification)?);
        self.write_tx
            .send(WriteRequest { line, ack: None })
            .await
            .map_err(|_| McpTransportError::ConnectionClosed)?;
        Ok(())
    }

    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
        progress_registration: Option<(ProgressTokenKey, mpsc::Sender<McpProgressUpdate>)>,
    ) -> Result<Value, McpTransportError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.send_request_with_id(id, method, params, progress_registration, None)
            .await
    }

    /// Send a JSON-RPC request using a caller-supplied id.
    ///
    /// `sent_flag`, when supplied, is set to `true` once the request
    /// has been enqueued for transmission (channel push for stdio,
    /// HTTP send returning OK for HTTP). The cancellable call path
    /// reads it before emitting `notifications/cancelled`: if the
    /// request never went out, the server hasn't allocated state for
    /// the id, so the spec-mandated notification would reference a
    /// requestId the peer never saw — confusing logs at best, an
    /// "unknown requestId" rejection at worst.
    async fn send_request_with_id(
        &self,
        id: i64,
        method: &str,
        params: Option<Value>,
        progress_registration: Option<(ProgressTokenKey, mpsc::Sender<McpProgressUpdate>)>,
        sent_flag: Option<Arc<AtomicBool>>,
    ) -> Result<Value, McpTransportError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }

        let request = JsonRpcRequest::new(JsonRpcId::Number(id), method.to_string(), params);
        let line = format!("{}\n", serde_json::to_string(&request)?);

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let progress_key = progress_registration.as_ref().map(|(key, _)| key.clone());
        if let Some((key, sender)) = progress_registration {
            self.progress_subscribers.lock().await.insert(key, sender);
        }

        if self
            .write_tx
            .send(WriteRequest { line, ack: None })
            .await
            .is_err()
        {
            self.pending.lock().await.remove(&id);
            if let Some(key) = progress_key {
                self.progress_subscribers.lock().await.remove(&key);
            }
            return Err(McpTransportError::ConnectionClosed);
        }

        // Channel push succeeded — the writer task will flush this
        // line; ordering between the request and any subsequent
        // notification (e.g. cancellation) is preserved via the
        // single-consumer write channel.
        if let Some(flag) = sent_flag.as_ref() {
            flag.store(true, Ordering::SeqCst);
        }

        let response = tokio::time::timeout(self.timeout, rx).await;
        if let Some(key) = progress_key {
            self.progress_subscribers.lock().await.remove(&key);
        }

        match response {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                Err(McpTransportError::ConnectionClosed)
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(McpTransportError::Timeout(format!(
                    "Request timed out after {:?}",
                    self.timeout
                )))
            }
        }
    }

    /// Drop a pending request entry on cancellation so the reader task
    /// doesn't keep the channel alive. The matching response (if it ever
    /// arrives) will then be silently discarded.
    async fn forget_pending(&self, id: i64) {
        self.pending.lock().await.remove(&id);
    }

    /// Send a JSON-RPC notification and wait for the writer task to
    /// confirm the line has been written + flushed to subprocess stdin.
    /// Critical for the cancellation path: the transport is dropped
    /// immediately after this returns, which triggers `kill_on_drop`
    /// on the subprocess — without the ack we'd race the kill and the
    /// `notifications/cancelled` might never reach the server.
    async fn send_notification_flushed(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), McpTransportError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }
        let notification = JsonRpcNotification::new(method, params);
        let line = format!("{}\n", serde_json::to_string(&notification)?);
        let (ack_tx, ack_rx) = oneshot::channel();
        self.write_tx
            .send(WriteRequest {
                line,
                ack: Some(ack_tx),
            })
            .await
            .map_err(|_| McpTransportError::ConnectionClosed)?;
        // Bounded wait — if the writer task is gone the ack will drop.
        let _ = tokio::time::timeout(Duration::from_secs(2), ack_rx).await;
        Ok(())
    }
}

#[async_trait]
impl McpToolTransport for ProgressAwareStdioTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        let result = self
            .send_request("tools/list", Some(json!({})), None)
            .await?;
        let list_result: ListToolsResult = serde_json::from_value(result)?;
        Ok(list_result.tools)
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
        let result = self
            .send_request("prompts/list", Some(json!({})), None)
            .await?;
        let list_result: ListPromptsResult = serde_json::from_value(result)?;
        Ok(list_result.prompts)
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<McpPromptResult, McpTransportError> {
        let result = self
            .send_request(
                "prompts/get",
                Some(json!({
                    "name": name,
                    "arguments": arguments,
                })),
                None,
            )
            .await?;
        serde_json::from_value(result).map_err(Into::into)
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
        let result = self
            .send_request("resources/list", Some(json!({})), None)
            .await?;
        let list_result: ListResourcesResult = serde_json::from_value(result)?;
        Ok(list_result.resources)
    }

    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
        context: McpCallContext,
    ) -> Result<CallToolResult, McpTransportError> {
        let McpCallContext {
            metadata,
            cancellation,
            sampling,
        } = context;

        // Pre-check cancellation BEFORE allocating the request id, the
        // progress token, or the per-call sampling slot. Without this,
        // an already-cancelled caller would still allocate a fresh id,
        // emit `notifications/cancelled` for an id the server never
        // saw, and pollute counters.
        if let Some(ref token) = cancellation
            && token.is_cancelled()
        {
            return Err(McpTransportError::TransportError(
                CANCELLED_BY_CLIENT.to_string(),
            ));
        }

        let (progress_token, progress_sender) = match progress_tx {
            Some(sender) => {
                let token =
                    ProgressToken::Number(self.next_progress_token.fetch_add(1, Ordering::SeqCst));
                let key = ProgressTokenKey::from(&token);
                (Some(token), Some((key, sender)))
            }
            None => (None, None),
        };

        let meta = build_call_tool_meta(progress_token, &metadata)?;

        let params = CallToolParams {
            name: name.to_string(),
            arguments: Some(args),
            task: None,
            meta,
        };

        // Allocate the request id up front so notifications/cancelled can
        // reference it on cancellation. Without this, the id is generated
        // inside `send_request` and there's no way to address the
        // in-flight call from outside.
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        // Register the per-call sampling decision deterministically
        // BEFORE the request hits the wire. Awaiting the lock here is
        // required: if registration raced the server-initiated
        // sampling/createMessage we want to route to this entry, an
        // earlier try_lock-based scheme would silently miss it.
        let mut handler_guard =
            PerCallSamplingGuard::register(Arc::clone(&self.per_call_sampling), id, sampling).await;

        let request_sent = Arc::new(AtomicBool::new(false));
        let request_fut = self.send_request_with_id(
            id,
            "tools/call",
            Some(serde_json::to_value(&params)?),
            progress_sender,
            Some(Arc::clone(&request_sent)),
        );

        let result = match cancellation {
            None => request_fut.await,
            Some(token) => {
                tokio::pin!(request_fut);
                tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        // Per spec (2025-11-25 §Cancellation): the client
                        // SHOULD send `notifications/cancelled` with the
                        // in-flight requestId so the server can stop
                        // processing and free resources. We use the
                        // flushed variant because the transport is
                        // typically dropped immediately after this
                        // returns; `kill_on_drop(true)` would race the
                        // notification write and the server might never
                        // see the cancellation.
                        //
                        // Only emit when the request actually went out.
                        // If cancellation fired before `request_fut` had
                        // a chance to push the request line onto the
                        // write channel, the server never allocated
                        // state for this requestId — emitting the
                        // notification would reference an unknown id.
                        if request_sent.load(Ordering::SeqCst) {
                            let _ = self
                                .send_notification_flushed(
                                    "notifications/cancelled",
                                    Some(json!({
                                        "requestId": id,
                                        "reason": "client run cancelled",
                                    })),
                                )
                                .await;
                        }
                        // Drop the pending entry so a late response is
                        // dropped on the reader floor rather than
                        // delivered to an orphaned channel.
                        self.forget_pending(id).await;
                        // Deterministic guard cleanup before return so
                        // no request-bound sampling entry survives this
                        // call. The sync Drop is a backstop only.
                        handler_guard.unregister().await;
                        return Err(McpTransportError::TransportError(
                            CANCELLED_BY_CLIENT.to_string(),
                        ));
                    }
                    result = &mut request_fut => result,
                }
            }
        };

        // Deterministic guard cleanup on every non-cancel return path,
        // before we transform `result` into the final value the caller
        // sees. Without this, a fast follow-up call could observe a
        // map entry from the just-returned call and route sampling to
        // a stale handler.
        handler_guard.unregister().await;

        let result = result?;
        serde_json::from_value(result).map_err(Into::into)
    }

    fn transport_type(&self) -> TransportTypeId {
        TransportTypeId::Stdio
    }

    async fn server_capabilities(&self) -> Result<Option<ServerCapabilities>, McpTransportError> {
        Ok(self.capabilities.clone())
    }

    async fn take_list_changed_receiver(&self) -> Option<mpsc::Receiver<ListChangedKind>> {
        self.list_changed_rx.lock().await.take()
    }

    async fn take_resource_updated_receiver(&self) -> Option<mpsc::Receiver<String>> {
        self.resource_updated_rx.lock().await.take()
    }

    async fn subscribe_resource(&self, uri: &str) -> Result<(), McpTransportError> {
        self.send_request("resources/subscribe", Some(json!({ "uri": uri })), None)
            .await
            .map(|_| ())
    }

    async fn unsubscribe_resource(&self, uri: &str) -> Result<(), McpTransportError> {
        self.send_request("resources/unsubscribe", Some(json!({ "uri": uri })), None)
            .await
            .map(|_| ())
    }

    async fn complete(&self, params: CompleteParams) -> Result<CompleteResult, McpTransportError> {
        let result = self
            .send_request(
                "completion/complete",
                Some(serde_json::to_value(&params)?),
                None,
            )
            .await?;
        serde_json::from_value(result).map_err(Into::into)
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, McpTransportError> {
        self.send_request("resources/read", Some(json!({ "uri": uri })), None)
            .await
    }

    async fn close(&self) -> Result<(), McpTransportError> {
        self.alive.store(false, Ordering::SeqCst);

        {
            let mut pending = self.pending.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(McpTransportError::ConnectionClosed));
            }
        }

        {
            let mut progress = self.progress_subscribers.lock().await;
            progress.clear();
        }

        let child = {
            let mut child_guard = self.child.lock().await;
            child_guard.take()
        };

        if let Some(mut child) = child {
            terminate_child(&mut child).await?;
        }

        Ok(())
    }
}

// ── HTTP transport ──

pub(crate) struct ProgressAwareHttpTransport {
    endpoint: String,
    client: reqwest::Client,
    streaming_client: reqwest::Client,
    timeout: Duration,
    next_id: AtomicI64,
    next_progress_token: AtomicI64,
    capabilities: Arc<tokio::sync::Mutex<Option<ServerCapabilities>>>,
    initialize_lock: tokio::sync::Mutex<()>,
    session: Arc<tokio::sync::RwLock<HttpSessionState>>,
    sampling_handler: Option<Arc<dyn SamplingHandler>>,
    /// Per-call sampling handlers for HTTP request-bound SSE streams.
    /// Populated by `call_tool` while a request is in flight; consulted
    /// by `process_sse_event` when `sampling/createMessage` arrives on
    /// that same request stream.
    per_call_sampling: PerCallSamplingHandlers,
    /// Sender for `notifications/.../list_changed` events observed on
    /// the HTTP SSE listening stream (and on per-request SSE streams).
    /// Cloned into the listening-stream task at the point we add one;
    /// for now, [`process_sse_event`] dispatches synchronously through
    /// this sender so per-request streams report list_changed too.
    list_changed_tx: mpsc::Sender<ListChangedKind>,
    /// One-shot receiver, taken at most once by
    /// [`take_list_changed_receiver`].
    list_changed_rx: tokio::sync::Mutex<Option<mpsc::Receiver<ListChangedKind>>>,
    /// Sender for `notifications/resources/updated` events; cloned by
    /// `process_sse_event` to forward each notification.
    resource_updated_tx: mpsc::Sender<String>,
    /// One-shot receiver for resource-updated events; same pattern as
    /// `list_changed_rx`.
    resource_updated_rx: tokio::sync::Mutex<Option<mpsc::Receiver<String>>>,
    /// Client-defined roots; same semantics as the stdio variant.
    /// Non-empty → roots capability advertised at initialize and
    /// `roots/list` is served from this list.
    roots: Arc<Vec<Root>>,
    /// Whether to advertise `sampling` capability at initialize. This
    /// is true only when the manager has a transport-level fallback
    /// handler that can answer server requests without per-call
    /// attribution; factory-only routing intentionally leaves it false.
    advertise_sampling: bool,
    /// Tracks whether we've spawned the background GET listening
    /// stream task yet (spec 2025-11-25 §Streamable HTTP / Listening
    /// for Messages from the Server). Start-once, never restart from
    /// here — the task itself loops over connect / reconnect /
    /// backoff. Set via `swap` so concurrent first-initialize callers
    /// don't double-spawn.
    listener_started: AtomicBool,
    /// `AbortHandle` for the listener task. Held so `close()` (and
    /// the Drop impl) can cancel the task immediately rather than
    /// leaking it past the transport's lifetime. `None` before the
    /// task is spawned; cleared on `close()`.
    listener_abort: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
    /// Atomic kill switch the listener task polls between SSE chunks
    /// so a clean `close()` shuts it down even if `AbortHandle::abort`
    /// races a pending request.
    listener_alive: Arc<AtomicBool>,
    /// `close()` is terminal for HTTP transports. Manager lifecycle
    /// code drops closed runtimes and creates a fresh transport for
    /// reconnect/re-enable, which avoids ambiguous listener restart
    /// semantics after session teardown.
    closed: AtomicBool,
}

#[derive(Debug, Clone, Default)]
struct HttpSessionState {
    session_id: Option<String>,
    protocol_version: Option<String>,
    started_at: Option<SystemTime>,
    generation: u64,
}

#[derive(Debug, Clone, Default)]
struct HttpSessionSnapshot {
    session_id: Option<String>,
    protocol_version: Option<String>,
    generation: u64,
}

impl HttpSessionState {
    fn snapshot(&self) -> HttpSessionSnapshot {
        HttpSessionSnapshot {
            session_id: self.session_id.clone(),
            protocol_version: self.protocol_version.clone(),
            generation: self.generation,
        }
    }
}

#[derive(Debug)]
struct HttpPostResponse {
    response: reqwest::Response,
    session: HttpSessionSnapshot,
}

async fn reset_http_session_state_if_current(
    session: &Arc<tokio::sync::RwLock<HttpSessionState>>,
    capabilities: &Arc<tokio::sync::Mutex<Option<ServerCapabilities>>>,
    expected_session_id: Option<&str>,
    expected_protocol_version: Option<&str>,
    expected_generation: u64,
) -> bool {
    // Lock capabilities first to match initialize_if_needed's ordering.
    // This keeps "clear caps + clear session" atomic from request paths
    // that would otherwise see stale capabilities with an empty session.
    let mut capabilities_guard = capabilities.lock().await;
    let mut session_guard = session.write().await;
    if session_guard.session_id.as_deref() != expected_session_id
        || session_guard.protocol_version.as_deref() != expected_protocol_version
        || session_guard.generation != expected_generation
    {
        return false;
    }
    *session_guard = HttpSessionState {
        generation: session_guard.generation.saturating_add(1),
        ..HttpSessionState::default()
    };
    *capabilities_guard = None;
    true
}

impl ProgressAwareHttpTransport {
    pub(crate) fn connect(
        config: &McpServerConnectionConfig,
        sampling_handler: Option<Arc<dyn SamplingHandler>>,
        roots: Arc<Vec<Root>>,
        advertise_sampling: bool,
    ) -> Result<Self, McpTransportError> {
        let endpoint = config.url.as_ref().ok_or_else(|| {
            McpTransportError::TransportError("HTTP transport requires URL".to_string())
        })?;
        let timeout = Duration::from_secs(config.timeout_secs);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| {
                McpTransportError::TransportError(format!("Failed to create HTTP client: {}", e))
            })?;
        let streaming_client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .build()
            .map_err(|e| {
                McpTransportError::TransportError(format!(
                    "Failed to create HTTP streaming client: {}",
                    e
                ))
            })?;

        let (list_changed_tx, list_changed_rx) =
            mpsc::channel::<ListChangedKind>(LIST_CHANGED_CHANNEL_CAPACITY);
        let (resource_updated_tx, resource_updated_rx) =
            mpsc::channel::<String>(RESOURCE_UPDATED_INGRESS_CAPACITY);
        Ok(Self {
            endpoint: endpoint.clone(),
            client,
            streaming_client,
            timeout,
            next_id: AtomicI64::new(1),
            next_progress_token: AtomicI64::new(1),
            capabilities: Arc::new(tokio::sync::Mutex::new(None)),
            initialize_lock: tokio::sync::Mutex::new(()),
            session: Arc::new(tokio::sync::RwLock::new(HttpSessionState::default())),
            sampling_handler,
            per_call_sampling: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            list_changed_tx,
            list_changed_rx: tokio::sync::Mutex::new(Some(list_changed_rx)),
            resource_updated_tx,
            resource_updated_rx: tokio::sync::Mutex::new(Some(resource_updated_rx)),
            roots,
            advertise_sampling,
            listener_started: AtomicBool::new(false),
            listener_abort: std::sync::Mutex::new(None),
            listener_alive: Arc::new(AtomicBool::new(true)),
            closed: AtomicBool::new(false),
        })
    }

    fn ensure_open(&self) -> Result<(), McpTransportError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }
        Ok(())
    }

    async fn initialize_if_needed(&self) -> Result<ServerCapabilities, McpTransportError> {
        self.ensure_open()?;
        {
            let guard = self.capabilities.lock().await;
            if let Some(capabilities) = guard.clone() {
                self.spawn_listening_stream_once();
                return Ok(capabilities);
            }
        }

        let _initialize_guard = self.initialize_lock.lock().await;
        {
            let guard = self.capabilities.lock().await;
            if let Some(capabilities) = guard.clone() {
                self.spawn_listening_stream_once();
                return Ok(capabilities);
            }
        }
        let capabilities = self.initialize().await?;
        let mut guard = self.capabilities.lock().await;
        *guard = Some(capabilities.clone());
        drop(guard);
        self.spawn_listening_stream_once();
        Ok(capabilities)
    }

    async fn initialize(&self) -> Result<ServerCapabilities, McpTransportError> {
        // Build advertised capabilities the same way the stdio path
        // does: sampling iff this transport has a handler that can
        // answer global server-initiated sampling requests, roots iff we
        // have roots to serve. Per-call factories are intentionally not
        // advertised as global sampling support; they are only safe on
        // per-request SSE streams where request_id binds the sampling
        // request to a specific agent call.
        let mut caps = InitializeCapabilities::default();
        if self.sampling_handler.is_some() || self.advertise_sampling {
            caps.sampling = Some(SamplingCapabilities::default());
        }
        if !self.roots.is_empty() {
            caps.roots = Some(mcp::transport::RootsCapabilities {
                list_changed: Some(false),
            });
        }
        let caps_value = serde_json::to_value(&caps).unwrap_or_else(|_| json!({}));
        let request_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let post_response = self
            .post_message(
                JsonRpcMessage::Request(JsonRpcRequest::new(
                    JsonRpcId::Number(request_id),
                    "initialize".to_string(),
                    Some(initialize_params(caps_value, Value::Null)),
                )),
                true,
            )
            .await?;

        // Spec literal: `MCP-Session-Id`. HeaderMap::get is
        // case-insensitive so this works regardless of how the server
        // capitalises the response header.
        let session_id = post_response
            .response
            .headers()
            .get("MCP-Session-Id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        let body = match self
            .decode_initialize_body(post_response.response, request_id, session_id.as_deref())
            .await
        {
            Ok(body) => body,
            Err(err) => {
                if let Some(session_id) = session_id.as_deref() {
                    self.send_session_termination(session_id, None).await;
                }
                return Err(McpTransportError::ProtocolError(format!(
                    "initialize failed: {err}"
                )));
            }
        };
        let result: InitializeResult = match serde_json::from_value(body) {
            Ok(result) => result,
            Err(err) => {
                if let Some(session_id) = session_id.as_deref() {
                    self.send_session_termination(session_id, None).await;
                }
                return Err(err.into());
            }
        };

        // Validate the server-echoed protocolVersion BEFORE accepting
        // the session id or emitting `notifications/initialized`. On
        // mismatch we leave session state empty so the next attempt
        // re-handshakes rather than half-living with an unusable session.
        let negotiated = match negotiate_protocol_version(&result.protocol_version) {
            Ok(negotiated) => negotiated,
            Err(err) => {
                if let Some(session_id) = session_id.as_deref() {
                    self.send_session_termination(session_id, None).await;
                }
                return Err(err);
            }
        };

        let installed_generation = {
            let mut session = self.session.write().await;
            let installed_generation = session.generation.saturating_add(1);
            session.session_id = session_id.clone();
            session.protocol_version = Some(negotiated.to_string());
            session.started_at = Some(SystemTime::now());
            session.generation = installed_generation;
            installed_generation
        };

        if let Err(err) = self
            .send_notification("notifications/initialized", Some(json!({})))
            .await
        {
            reset_http_session_state_if_current(
                &self.session,
                &self.capabilities,
                session_id.as_deref(),
                Some(negotiated),
                installed_generation,
            )
            .await;
            if let Some(session_id) = session_id.as_deref() {
                self.send_session_termination(session_id, Some(negotiated))
                    .await;
            }
            return Err(err);
        }

        Ok(result.capabilities)
    }

    /// Spawn the background GET listener task at most once per
    /// transport instance. Idempotent: subsequent calls are no-ops.
    fn spawn_listening_stream_once(&self) {
        if self.listener_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let client = self.client.clone();
        let streaming_client = self.streaming_client.clone();
        let endpoint = self.endpoint.clone();
        let session = Arc::clone(&self.session);
        let capabilities = Arc::clone(&self.capabilities);
        let list_changed_tx = self.list_changed_tx.clone();
        let resource_updated_tx = self.resource_updated_tx.clone();
        let roots = Arc::clone(&self.roots);
        let fallback_sampling = self.sampling_handler.clone();
        let alive = Arc::clone(&self.listener_alive);

        let handle = tokio::spawn(run_http_listening_stream(HttpListenerCtx {
            client,
            streaming_client,
            timeout: self.timeout,
            endpoint,
            session,
            capabilities,
            list_changed_tx,
            resource_updated_tx,
            roots,
            fallback_sampling,
            alive,
        }));
        if let Ok(mut slot) = self.listener_abort.lock() {
            *slot = Some(handle.abort_handle());
        }
    }

    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
        progress_registration: Option<(ProgressTokenKey, mpsc::Sender<McpProgressUpdate>)>,
    ) -> Result<Value, McpTransportError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.send_request_with_id(id, method, params, progress_registration, None, None)
            .await
    }

    /// Variant of `send_request` that uses a caller-allocated id so the
    /// cancellable call path can emit `notifications/cancelled` against
    /// the same in-flight request id.
    ///
    /// `sent_flag` is an optimistic "believed in-progress" marker. It is
    /// set after the HTTP session snapshot has been captured but before
    /// awaiting response headers, because that wait can last for the
    /// whole tool execution. `sent_session`, when supplied, receives the
    /// exact session snapshot used for the POST so cancellation can send
    /// `notifications/cancelled` to the original session instead of a
    /// newer session installed by a concurrent reset/reinitialize.
    async fn send_request_with_id(
        &self,
        id: i64,
        method: &str,
        params: Option<Value>,
        progress_registration: Option<(ProgressTokenKey, mpsc::Sender<McpProgressUpdate>)>,
        sent_flag: Option<Arc<AtomicBool>>,
        sent_session: Option<Arc<tokio::sync::Mutex<Option<HttpSessionSnapshot>>>>,
    ) -> Result<Value, McpTransportError> {
        let request = JsonRpcRequest::new(JsonRpcId::Number(id), method.to_string(), params);
        let session = self.session.read().await.snapshot();
        if let Some(slot) = sent_session.as_ref() {
            *slot.lock().await = Some(session.clone());
        }
        // Mark the request as "believed in-progress" BEFORE awaiting
        // response headers. That await can last for the whole
        // per-request SSE stream — many seconds for a long-running
        // tool. Setting the flag only after that window would make the
        // cancellation branch skip notifications/cancelled for the
        // most-important case (slow tool call needs cancelling).
        // Per MCP 2025-11-25 §Cancellation: unknown requestIds on
        // `notifications/cancelled` SHOULD be ignored by the receiver,
        // so being slightly optimistic here is safe.
        if let Some(flag) = sent_flag.as_ref() {
            flag.store(true, Ordering::SeqCst);
        }
        let post_response = self
            .post_message_with_session_snapshot(JsonRpcMessage::Request(request), true, session)
            .await?;
        self.decode_http_body(
            post_response.response,
            id,
            progress_registration,
            Some(post_response.session),
        )
        .await
    }

    async fn send_initialized_request(
        &self,
        method: &str,
        params: Option<Value>,
        progress_registration: Option<(ProgressTokenKey, mpsc::Sender<McpProgressUpdate>)>,
    ) -> Result<Value, McpTransportError> {
        self.initialize_if_needed().await?;
        match self
            .send_request(method, params.clone(), progress_registration.clone())
            .await
        {
            Ok(value) => Ok(value),
            Err(McpTransportError::ProtocolError(message)) if message == MCP_SESSION_EXPIRED => {
                self.initialize_if_needed().await?;
                self.send_request(method, params, progress_registration)
                    .await
            }
            Err(err) => Err(err),
        }
    }

    async fn post_message(
        &self,
        message: JsonRpcMessage,
        expect_response: bool,
    ) -> Result<HttpPostResponse, McpTransportError> {
        let session = self.session.read().await.snapshot();
        self.post_message_with_session_snapshot(message, expect_response, session)
            .await
    }

    async fn post_message_with_session_snapshot(
        &self,
        message: JsonRpcMessage,
        expect_response: bool,
        session: HttpSessionSnapshot,
    ) -> Result<HttpPostResponse, McpTransportError> {
        self.ensure_open()?;
        let is_initialize = matches!(
            &message,
            JsonRpcMessage::Request(request) if request.method == "initialize"
        );
        let client = if expect_response {
            &self.streaming_client
        } else {
            &self.client
        };
        let mut request = client.post(&self.endpoint).header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        );

        let sent_session_id = session.session_id.clone();
        let sent_protocol_version = session.protocol_version.clone();
        if !is_initialize && sent_protocol_version.is_none() {
            return Err(McpTransportError::ProtocolError(
                MCP_SESSION_EXPIRED.to_string(),
            ));
        }
        if let Some(ref protocol_version) = sent_protocol_version {
            request = request.header("MCP-Protocol-Version", protocol_version.clone());
        }
        if let Some(ref session_id) = sent_session_id {
            request = request.header("MCP-Session-Id", session_id.clone());
        }

        request = match message {
            JsonRpcMessage::Request(request_body) => request.json(&request_body),
            JsonRpcMessage::Notification(notification) => request.json(&notification),
            JsonRpcMessage::Response(response) => request.json(&response),
        };

        let response = tokio::time::timeout(self.timeout, request.send())
            .await
            .map_err(|_| {
                McpTransportError::Timeout(format!(
                    "HTTP request did not receive response headers within {:?}",
                    self.timeout
                ))
            })?
            .map_err(|e| {
                McpTransportError::TransportError(format!("HTTP request failed: {}", e))
            })?;

        let status = response.status();
        if !status.is_success() {
            if status == reqwest::StatusCode::NOT_FOUND && sent_session_id.is_some() {
                reset_http_session_state_if_current(
                    &self.session,
                    &self.capabilities,
                    sent_session_id.as_deref(),
                    sent_protocol_version.as_deref(),
                    session.generation,
                )
                .await;
                return Err(McpTransportError::ProtocolError(
                    MCP_SESSION_EXPIRED.to_string(),
                ));
            }

            let body = tokio::time::timeout(self.timeout, response.text())
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            return Err(McpTransportError::TransportError(format!(
                "HTTP error: {} - {}",
                status, body
            )));
        }

        if !expect_response
            && status != reqwest::StatusCode::ACCEPTED
            && status != reqwest::StatusCode::NO_CONTENT
            && status != reqwest::StatusCode::OK
        {
            return Err(McpTransportError::ProtocolError(format!(
                "Expected 202 Accepted for HTTP notification/response, got {}",
                status
            )));
        }

        Ok(HttpPostResponse { response, session })
    }

    async fn send_notification(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), McpTransportError> {
        let notification = JsonRpcNotification::new(method, params);
        self.post_message(JsonRpcMessage::Notification(notification), false)
            .await?;
        Ok(())
    }

    async fn send_notification_with_session(
        &self,
        method: &str,
        params: Option<Value>,
        request_session: &HttpSessionSnapshot,
    ) -> Result<(), McpTransportError> {
        let current_session = self.session.read().await.snapshot();
        if current_session.generation != request_session.generation
            || current_session.session_id != request_session.session_id
            || current_session.protocol_version != request_session.protocol_version
        {
            tracing::debug!(
                request_generation = request_session.generation,
                current_generation = current_session.generation,
                "sending MCP notification with captured HTTP session because current session changed"
            );
        }

        let notification = JsonRpcNotification::new(method, params);
        self.post_message_with_session_snapshot(
            JsonRpcMessage::Notification(notification),
            false,
            request_session.clone(),
        )
        .await?;
        Ok(())
    }

    async fn send_response_message_with_session(
        &self,
        response: JsonRpcResponse,
        request_session: &HttpSessionSnapshot,
    ) -> Result<(), McpTransportError> {
        self.ensure_current_session_matches(request_session).await?;

        match self
            .post_message_with_session_snapshot(
                JsonRpcMessage::Response(response),
                false,
                request_session.clone(),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(McpTransportError::ProtocolError(message)) if message == MCP_SESSION_EXPIRED => {
                Err(McpTransportError::ProtocolError(
                    MCP_SESSION_EXPIRED_AFTER_ACCEPT.to_string(),
                ))
            }
            Err(err) => Err(err),
        }
    }

    async fn ensure_current_session_matches(
        &self,
        request_session: &HttpSessionSnapshot,
    ) -> Result<(), McpTransportError> {
        let current_session = self.session.read().await.snapshot();
        if current_session.generation != request_session.generation
            || current_session.session_id != request_session.session_id
            || current_session.protocol_version != request_session.protocol_version
        {
            tracing::debug!(
                stream_generation = request_session.generation,
                current_generation = current_session.generation,
                "dropping MCP per-request SSE response for stale HTTP session generation"
            );
            return Err(McpTransportError::ProtocolError(
                MCP_SESSION_EXPIRED_AFTER_ACCEPT.to_string(),
            ));
        }

        Ok(())
    }

    async fn decode_initialize_body(
        &self,
        response: reqwest::Response,
        request_id: i64,
        provisional_session_id: Option<&str>,
    ) -> Result<Value, McpTransportError> {
        let content_type = response_content_type(&response);

        if content_type.starts_with("application/json") {
            let body = decode_json_response_body(response, self.timeout).await?;
            return decode_http_response_payload(body, request_id, None);
        }

        if content_type.starts_with("text/event-stream") {
            return self
                .decode_initialize_sse_response(response, request_id, provisional_session_id)
                .await;
        }

        Err(McpTransportError::ProtocolError(format!(
            "Unsupported HTTP content type: {}",
            content_type
        )))
    }

    async fn decode_initialize_sse_response(
        &self,
        response: reqwest::Response,
        request_id: i64,
        provisional_session_id: Option<&str>,
    ) -> Result<Value, McpTransportError> {
        let mut last_event_id: Option<String> = None;
        let mut retry_delay: Option<Duration> = None;
        let mut resume_attempts = 0u32;
        let mut current_response = response;

        loop {
            let mut matched_response: Option<Result<Value, McpTransportError>> = None;
            let mut event_lines: Vec<String> = Vec::new();
            let mut event_bytes = 0usize;
            let mut line_buf: Vec<u8> = Vec::new();
            let mut stream_io_error: Option<reqwest::Error> = None;
            let mut stream = current_response.bytes_stream();

            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        stream_io_error = Some(err);
                        break;
                    }
                };

                for byte in chunk {
                    if byte == b'\n' {
                        let mut line =
                            String::from_utf8(std::mem::take(&mut line_buf)).map_err(|e| {
                                McpTransportError::ProtocolError(format!(
                                    "Invalid UTF-8 in initialize SSE response: {}",
                                    e
                                ))
                            })?;
                        if line.ends_with('\r') {
                            line.pop();
                        }

                        if line.is_empty() {
                            apply_event_id_update(
                                &mut last_event_id,
                                extract_event_id(&event_lines),
                            );
                            if let Some(result) =
                                self.process_initialize_sse_event(&event_lines, request_id)?
                            {
                                matched_response = Some(result);
                                break;
                            }
                            if let Some(delay) =
                                bounded_sse_retry_delay(&event_lines, "initialize SSE response")?
                            {
                                retry_delay = Some(delay);
                            }
                            event_lines.clear();
                            event_bytes = 0;
                        } else {
                            push_sse_event_line(
                                &mut event_lines,
                                &mut event_bytes,
                                line,
                                "initialize SSE response",
                            )?;
                        }
                    } else {
                        push_sse_line_byte(&mut line_buf, byte, "initialize SSE response")?;
                    }
                }

                if matched_response.is_some() {
                    break;
                }
            }

            if !line_buf.is_empty() {
                let mut line = String::from_utf8(std::mem::take(&mut line_buf)).map_err(|e| {
                    McpTransportError::ProtocolError(format!(
                        "Invalid UTF-8 in initialize SSE response: {}",
                        e
                    ))
                })?;
                if line.ends_with('\r') {
                    line.pop();
                }
                push_sse_event_line(
                    &mut event_lines,
                    &mut event_bytes,
                    line,
                    "initialize SSE response",
                )?;
            }

            if matched_response.is_none() && !event_lines.is_empty() && stream_io_error.is_none() {
                apply_event_id_update(&mut last_event_id, extract_event_id(&event_lines));
                matched_response = self.process_initialize_sse_event(&event_lines, request_id)?;
                if matched_response.is_none()
                    && let Some(delay) =
                        bounded_sse_retry_delay(&event_lines, "initialize SSE response")?
                {
                    retry_delay = Some(delay);
                }
            }

            if let Some(result) = matched_response {
                return result;
            }

            if resume_attempts < MAX_SSE_RESUME_ATTEMPTS
                && let (Some(session_id), Some(id)) =
                    (provisional_session_id, last_event_id.clone())
            {
                resume_attempts += 1;
                if let Some(delay) = retry_delay {
                    tokio::time::sleep(delay).await;
                }
                tracing::debug!(
                    request_id,
                    attempt = resume_attempts,
                    last_event_id = %id,
                    clean_eof = stream_io_error.is_none(),
                    "initialize SSE stream ended before matched response; attempting Last-Event-ID resume"
                );
                match self.get_initialize_resume_stream(&id, session_id).await {
                    Ok(resumed) => {
                        current_response = resumed;
                        continue;
                    }
                    Err(resume_err) => {
                        if let Some(err) = stream_io_error {
                            tracing::warn!(
                                error = %resume_err,
                                "initialize Last-Event-ID resume failed; surfacing original stream error"
                            );
                            return Err(McpTransportError::TransportError(format!(
                                "Failed to read initialize SSE response body: {}",
                                err
                            )));
                        }
                        return Err(resume_err);
                    }
                }
            }

            if let Some(err) = stream_io_error {
                return Err(McpTransportError::TransportError(format!(
                    "Failed to read initialize SSE response body: {}",
                    err
                )));
            }

            return Err(McpTransportError::ProtocolError(format!(
                "Missing initialize response for request id {}",
                request_id
            )));
        }
    }

    fn process_initialize_sse_event(
        &self,
        lines: &[String],
        request_id: i64,
    ) -> Result<Option<Result<Value, McpTransportError>>, McpTransportError> {
        let Some(payload) = sse_data_payload(lines, "initialize SSE response")? else {
            return Ok(None);
        };

        let message =
            parse_json_rpc_message(serde_json::from_str::<Value>(&payload).map_err(|e| {
                McpTransportError::ProtocolError(format!(
                    "Invalid JSON payload in initialize SSE response: {}",
                    e
                ))
            })?)?;

        match message {
            JsonRpcMessage::Response(response) => {
                if matches!(response.id, JsonRpcId::Number(id) if id == request_id) {
                    Ok(Some(map_response_payload(response.payload)))
                } else {
                    Err(McpTransportError::ProtocolError(format!(
                        "initialize SSE stream for request id {request_id} returned response for different id {:?}",
                        response.id
                    )))
                }
            }
            JsonRpcMessage::Notification(_) => Ok(None),
            JsonRpcMessage::Request(request) => Err(McpTransportError::ProtocolError(format!(
                "initialize SSE stream returned server request before initialization completed: {}",
                request.method
            ))),
        }
    }

    async fn get_initialize_resume_stream(
        &self,
        last_event_id: &str,
        session_id: &str,
    ) -> Result<reqwest::Response, McpTransportError> {
        let request = self
            .streaming_client
            .get(&self.endpoint)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header("MCP-Session-Id", session_id)
            .header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
            .header("Last-Event-ID", last_event_id);

        let response = tokio::time::timeout(self.timeout, request.send())
            .await
            .map_err(|_| {
                McpTransportError::Timeout(format!(
                    "initialize SSE resume GET did not receive response headers within {:?}",
                    self.timeout
                ))
            })?
            .map_err(|e| {
                McpTransportError::TransportError(format!(
                    "initialize SSE resume GET failed: {}",
                    e
                ))
            })?;

        if !response.status().is_success() {
            return Err(McpTransportError::TransportError(format!(
                "initialize SSE resume GET returned {}",
                response.status()
            )));
        }
        validate_sse_response(&response, "initialize SSE resume GET")?;
        Ok(response)
    }

    async fn decode_http_body(
        &self,
        response: reqwest::Response,
        request_id: i64,
        progress_registration: Option<(ProgressTokenKey, mpsc::Sender<McpProgressUpdate>)>,
        request_session: Option<HttpSessionSnapshot>,
    ) -> Result<Value, McpTransportError> {
        let content_type = response_content_type(&response);

        if content_type.starts_with("application/json") {
            let body = decode_json_response_body(response, self.timeout).await?;
            return decode_http_response_payload(body, request_id, progress_registration);
        }

        if content_type.starts_with("text/event-stream") {
            let request_session = request_session.ok_or_else(|| {
                McpTransportError::ProtocolError(
                    "SSE response missing HTTP session snapshot".to_string(),
                )
            })?;
            return self
                .decode_sse_response(response, request_id, progress_registration, request_session)
                .await;
        }

        Err(McpTransportError::ProtocolError(format!(
            "Unsupported HTTP content type: {}",
            content_type
        )))
    }

    async fn decode_sse_response(
        &self,
        response: reqwest::Response,
        request_id: i64,
        progress_registration: Option<(ProgressTokenKey, mpsc::Sender<McpProgressUpdate>)>,
        request_session: HttpSessionSnapshot,
    ) -> Result<Value, McpTransportError> {
        let progress_key = progress_registration.as_ref().map(|(key, _)| key.clone());
        let progress_tx = progress_registration.as_ref().map(|(_, tx)| tx.clone());

        // Per spec 2025-11-25 §Streamable HTTP / Resumability: track
        // the highest `id:` field we've seen so a mid-stream drop or a
        // clean server close before the final response can be resumed via
        // `GET <endpoint>` + `Last-Event-ID`.
        let mut last_event_id: Option<String> = None;
        let mut retry_delay: Option<Duration> = None;
        let mut resume_attempts = 0u32;
        let mut current_response = response;

        loop {
            let mut matched_response: Option<Result<Value, McpTransportError>> = None;
            let mut event_lines: Vec<String> = Vec::new();
            let mut event_bytes = 0usize;
            let mut line_buf: Vec<u8> = Vec::new();
            let mut stream_io_error: Option<reqwest::Error> = None;
            let mut stream = current_response.bytes_stream();

            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        // Mid-stream IO error. If we have a
                        // last_event_id and reconnect budget remains,
                        // we'll attempt resume below.
                        stream_io_error = Some(err);
                        break;
                    }
                };

                for byte in chunk {
                    if byte == b'\n' {
                        let mut line =
                            String::from_utf8(std::mem::take(&mut line_buf)).map_err(|e| {
                                McpTransportError::ProtocolError(format!(
                                    "Invalid UTF-8 in SSE response: {}",
                                    e
                                ))
                            })?;
                        if line.ends_with('\r') {
                            line.pop();
                        }

                        if line.is_empty() {
                            apply_event_id_update(
                                &mut last_event_id,
                                extract_event_id(&event_lines),
                            );
                            if let Some(result) = self
                                .process_sse_event(
                                    &event_lines,
                                    request_id,
                                    progress_key.as_ref(),
                                    progress_tx.as_ref(),
                                    &request_session,
                                )
                                .await?
                            {
                                matched_response = Some(result);
                                break;
                            }
                            if let Some(delay) =
                                bounded_sse_retry_delay(&event_lines, "HTTP SSE response")?
                            {
                                retry_delay = Some(delay);
                            }
                            event_lines.clear();
                            event_bytes = 0;
                        } else {
                            push_sse_event_line(
                                &mut event_lines,
                                &mut event_bytes,
                                line,
                                "HTTP SSE response",
                            )?;
                        }
                    } else {
                        push_sse_line_byte(&mut line_buf, byte, "HTTP SSE response")?;
                    }
                }

                if matched_response.is_some() {
                    break;
                }
            }

            // Final event without trailing blank-line terminator.
            if !line_buf.is_empty() {
                let mut line = String::from_utf8(std::mem::take(&mut line_buf)).map_err(|e| {
                    McpTransportError::ProtocolError(format!(
                        "Invalid UTF-8 in SSE response: {}",
                        e
                    ))
                })?;
                if line.ends_with('\r') {
                    line.pop();
                }
                push_sse_event_line(
                    &mut event_lines,
                    &mut event_bytes,
                    line,
                    "HTTP SSE response",
                )?;
            }
            if matched_response.is_none() && !event_lines.is_empty() && stream_io_error.is_none() {
                apply_event_id_update(&mut last_event_id, extract_event_id(&event_lines));
                matched_response = self
                    .process_sse_event(
                        &event_lines,
                        request_id,
                        progress_key.as_ref(),
                        progress_tx.as_ref(),
                        &request_session,
                    )
                    .await?;
                if matched_response.is_none()
                    && let Some(delay) = bounded_sse_retry_delay(&event_lines, "HTTP SSE response")?
                {
                    retry_delay = Some(delay);
                }
            }

            if let Some(result) = matched_response {
                return result;
            }

            // Stream ended without a matched response. MCP Streamable HTTP
            // permits the server to close an SSE stream after sending an
            // event id without terminating the logical stream; poll/resume
            // through GET + Last-Event-ID for both broken streams and clean
            // EOFs.
            if resume_attempts < MAX_SSE_RESUME_ATTEMPTS
                && let Some(id) = last_event_id.clone()
            {
                resume_attempts += 1;
                if let Some(delay) = retry_delay {
                    tokio::time::sleep(delay).await;
                }
                tracing::debug!(
                    request_id,
                    attempt = resume_attempts,
                    last_event_id = %id,
                    clean_eof = stream_io_error.is_none(),
                    "SSE stream ended before matched response; attempting Last-Event-ID resume"
                );
                match self
                    .get_listening_stream(Some(&id), Some(request_session.generation))
                    .await
                {
                    Ok(resumed) => {
                        current_response = resumed;
                        continue;
                    }
                    Err(resume_err) => {
                        if matches!(
                            &resume_err,
                            McpTransportError::ProtocolError(message)
                                if message == MCP_SESSION_EXPIRED
                        ) {
                            return Err(McpTransportError::ProtocolError(
                                MCP_SESSION_EXPIRED_AFTER_ACCEPT.to_string(),
                            ));
                        }
                        if let Some(err) = stream_io_error {
                            tracing::warn!(
                                error = %resume_err,
                                "Last-Event-ID resume failed; surfacing original stream error"
                            );
                            return Err(McpTransportError::TransportError(format!(
                                "Failed to read SSE response body: {}",
                                err
                            )));
                        }
                        return Err(resume_err);
                    }
                }
            }

            if let Some(err) = stream_io_error {
                return Err(McpTransportError::TransportError(format!(
                    "Failed to read SSE response body: {}",
                    err
                )));
            }

            return Err(McpTransportError::ProtocolError(format!(
                "Missing response for request id {}",
                request_id
            )));
        }
    }

    /// Issue a `GET <endpoint>` carrying the current session id and
    /// the supplied `Last-Event-ID`. Used by [`decode_sse_response`] to
    /// resume a broken per-request SSE stream. The server SHOULD reply
    /// with an SSE stream containing only events strictly newer than
    /// the supplied id (spec 2025-11-25 §Streamable HTTP / Resumability).
    async fn get_listening_stream(
        &self,
        last_event_id: Option<&str>,
        expected_generation: Option<u64>,
    ) -> Result<reqwest::Response, McpTransportError> {
        let mut request = self
            .streaming_client
            .get(&self.endpoint)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        let session = self.session.read().await.snapshot();
        let sent_session_id = session.session_id.clone();
        let sent_protocol_version = session.protocol_version.clone();
        if sent_protocol_version.is_none() {
            return Err(McpTransportError::ProtocolError(
                MCP_SESSION_EXPIRED.to_string(),
            ));
        }
        if expected_generation.is_some_and(|generation| generation != session.generation) {
            return Err(McpTransportError::ProtocolError(
                MCP_SESSION_EXPIRED.to_string(),
            ));
        }
        if let Some(ref session_id) = sent_session_id {
            request = request.header("MCP-Session-Id", session_id.clone());
        }
        if let Some(ref protocol_version) = sent_protocol_version {
            request = request.header("MCP-Protocol-Version", protocol_version.clone());
        }
        if let Some(id) = last_event_id {
            request = request.header("Last-Event-ID", id);
        }
        let resp = tokio::time::timeout(self.timeout, request.send())
            .await
            .map_err(|_| {
                McpTransportError::Timeout(format!(
                    "SSE resume GET did not receive response headers within {:?}",
                    self.timeout
                ))
            })?
            .map_err(|e| {
                McpTransportError::TransportError(format!("SSE resume GET failed: {}", e))
            })?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND && sent_session_id.is_some() {
            reset_http_session_state_if_current(
                &self.session,
                &self.capabilities,
                sent_session_id.as_deref(),
                sent_protocol_version.as_deref(),
                session.generation,
            )
            .await;
            return Err(McpTransportError::ProtocolError(
                MCP_SESSION_EXPIRED.to_string(),
            ));
        }
        if !resp.status().is_success() {
            return Err(McpTransportError::TransportError(format!(
                "SSE resume GET returned {}",
                resp.status()
            )));
        }
        validate_sse_response(&resp, "SSE resume GET")?;
        Ok(resp)
    }

    async fn process_sse_event(
        &self,
        lines: &[String],
        request_id: i64,
        progress_key: Option<&ProgressTokenKey>,
        progress_tx: Option<&mpsc::Sender<McpProgressUpdate>>,
        request_session: &HttpSessionSnapshot,
    ) -> Result<Option<Result<Value, McpTransportError>>, McpTransportError> {
        let Some(payload) = sse_data_payload(lines, "HTTP SSE response")? else {
            return Ok(None);
        };

        let message =
            parse_json_rpc_message(serde_json::from_str::<Value>(&payload).map_err(|e| {
                McpTransportError::ProtocolError(format!(
                    "Invalid JSON payload in HTTP SSE response: {}",
                    e
                ))
            })?)?;

        match message {
            JsonRpcMessage::Response(response) => {
                if matches!(response.id, JsonRpcId::Number(id) if id == request_id) {
                    return Ok(Some(map_response_payload(response.payload)));
                }
                Err(McpTransportError::ProtocolError(format!(
                    "SSE stream for request id {request_id} returned response for different id {:?}",
                    response.id
                )))
            }
            JsonRpcMessage::Notification(notification) => {
                if let Some(kind) = list_changed_method(&notification.method) {
                    // Forward to the host's `take_list_changed_receiver`
                    // consumer; best-effort if no one subscribed.
                    forward_list_changed(&self.list_changed_tx, kind);
                } else if let Some(uri) = resource_updated_uri(&notification) {
                    let _ = self.resource_updated_tx.try_send(uri);
                } else if let (Some(expected_key), Some(sender)) = (progress_key, progress_tx)
                    && let Some((key, update)) = decode_progress_notification(notification)
                    && key == *expected_key
                {
                    let _ = sender.try_send(update);
                }
                Ok(None)
            }
            JsonRpcMessage::Request(request) => {
                self.ensure_current_session_matches(request_session).await?;
                // HTTP per-request SSE stream: per spec 2025-11-25
                // §Listening for Messages from the Server, messages on
                // this stream "SHOULD relate to a single client request"
                // — namely OUR `request_id`. So a server-initiated
                // `sampling/createMessage` arriving here belongs to that
                // call. Route directly by request_id rather than guessing
                // via cardinality.
                //
                // Three states:
                //   - Bound(h)  → use this call's bound handler
                //   - Denied    → factory consulted, refused: reject
                //                 (never silently fall through to a
                //                 fallback that may belong to a
                //                 different agent — that's the leak the
                //                 per-call routing exists to prevent)
                //   - no entry  → Inherit semantics: caller did not
                //                 engage the factory, fall through to
                //                 the transport-level fixed handler
                let chosen: Option<Arc<dyn SamplingHandler>> = {
                    let map = self.per_call_sampling.lock().await;
                    match map.get(&request_id) {
                        Some(PerCallSamplingEntry::Bound(h)) => Some(Arc::clone(h)),
                        Some(PerCallSamplingEntry::Denied) => None,
                        None => self.sampling_handler.clone(),
                    }
                };
                let response =
                    handle_server_request(chosen.as_deref(), self.roots.as_slice(), &request).await;
                self.send_response_message_with_session(response, request_session)
                    .await?;
                Ok(None)
            }
        }
    }
}

#[cfg(unix)]
async fn terminate_child(child: &mut Child) -> Result<(), McpTransportError> {
    let Some(pid) = child.id() else {
        let _ = child.wait().await;
        return Ok(());
    };

    send_signal(pid, Signal::SIGINT)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    if child.try_wait()?.is_some() {
        let _ = child.wait().await;
        return Ok(());
    }

    send_signal(pid, Signal::SIGTERM)?;
    tokio::time::sleep(Duration::from_millis(400)).await;

    if child.try_wait()?.is_some() {
        let _ = child.wait().await;
        return Ok(());
    }

    child.start_kill()?;
    let _ = child.wait().await;
    Ok(())
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: Signal) -> Result<(), McpTransportError> {
    match kill(Pid::from_raw(pid as i32), signal) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(err) => Err(McpTransportError::TransportError(format!(
            "failed to send signal {:?} to pid {}: {}",
            signal, pid, err
        ))),
    }
}

#[cfg(not(unix))]
async fn terminate_child(child: &mut Child) -> Result<(), McpTransportError> {
    child.start_kill()?;
    let _ = child.wait().await;
    Ok(())
}

// ── HTTP listening stream (spec 2025-11-25 §Streamable HTTP / Listening) ──

/// State the listening-stream task needs. Held by value (each field
/// is `Clone` or `Arc`'d) so the task is fully self-contained — it
/// doesn't borrow from the transport, which lets the transport drop
/// while the task is alive without lifetime headaches.
struct HttpListenerCtx {
    client: reqwest::Client,
    streaming_client: reqwest::Client,
    timeout: Duration,
    endpoint: String,
    session: Arc<tokio::sync::RwLock<HttpSessionState>>,
    /// Same `Arc` the transport holds. Needed so the listener's 404
    /// path can clear it: `initialize_if_needed` short-circuits when
    /// `capabilities = Some`, so leaving it stale after a session
    /// expiry would let subsequent calls send post_message() without
    /// a session id (spec violation — server returns 400/404 again).
    capabilities: Arc<tokio::sync::Mutex<Option<ServerCapabilities>>>,
    list_changed_tx: mpsc::Sender<ListChangedKind>,
    resource_updated_tx: mpsc::Sender<String>,
    roots: Arc<Vec<Root>>,
    fallback_sampling: Option<Arc<dyn SamplingHandler>>,
    alive: Arc<AtomicBool>,
}

async fn run_http_listening_stream(ctx: HttpListenerCtx) {
    const BACKOFF_BASE_MS: u64 = 500;
    let mut backoff_ms: u64 = BACKOFF_BASE_MS;
    let mut last_event_id: Option<String> = None;
    let mut last_event_generation: Option<u64> = None;

    while ctx.alive.load(Ordering::SeqCst) {
        let session_snapshot = ctx.session.read().await.clone();
        if last_event_generation != Some(session_snapshot.generation) {
            // SSE event ids are scoped to the HTTP session/generation
            // that produced them. Another request path can reset and
            // re-initialize the session while this background listener
            // is sleeping or consuming an old stream; never replay that
            // old cursor against the fresh session.
            last_event_id = None;
            last_event_generation = Some(session_snapshot.generation);
        }
        if session_snapshot.protocol_version.is_none() {
            // A prior 404 cleared the HTTP session. The request path
            // owns re-initialize; the background listener must not send
            // unauthenticated/unversioned GETs while waiting for that.
            tokio::time::sleep(Duration::from_millis(BACKOFF_BASE_MS)).await;
            continue;
        }
        let mut req = ctx
            .streaming_client
            .get(&ctx.endpoint)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if let Some(ref id) = session_snapshot.session_id {
            req = req.header("MCP-Session-Id", id.clone());
        }
        if let Some(ref pv) = session_snapshot.protocol_version {
            req = req.header("MCP-Protocol-Version", pv.clone());
        }
        if let Some(ref id) = last_event_id {
            req = req.header("Last-Event-ID", id.clone());
        }

        let response = match tokio::time::timeout(ctx.timeout, req.send()).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(err)) => {
                tracing::debug!(error = %err, "MCP listening stream GET failed; backing off");
                sleep_with_backoff(&mut backoff_ms).await;
                continue;
            }
            Err(_) => {
                tracing::debug!(
                    timeout = ?ctx.timeout,
                    "MCP listening stream GET timed out waiting for response headers; backing off"
                );
                sleep_with_backoff(&mut backoff_ms).await;
                continue;
            }
        };

        let status = response.status();
        if status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            tracing::debug!(
                "MCP server does not support GET listening stream (405); stopping listener"
            );
            break;
        }
        if status == reqwest::StatusCode::NOT_FOUND && session_snapshot.session_id.is_some() {
            // Session expired. Clear BOTH cached session AND cached
            // capabilities — without the second, `initialize_if_needed`
            // would short-circuit on next request-path call (capabilities
            // = Some) and `post_message` would then run without a
            // session id, getting another 404. Clearing capabilities
            // forces the next request to walk the full handshake.
            tracing::debug!(
                "MCP listening stream got 404 — resetting session + capabilities to force re-handshake"
            );
            reset_http_session_state_if_current(
                &ctx.session,
                &ctx.capabilities,
                session_snapshot.session_id.as_deref(),
                session_snapshot.protocol_version.as_deref(),
                session_snapshot.generation,
            )
            .await;
            last_event_id = None;
            sleep_with_backoff(&mut backoff_ms).await;
            continue;
        }
        if !status.is_success() {
            tracing::debug!(%status, "MCP listening stream non-success status; backing off");
            sleep_with_backoff(&mut backoff_ms).await;
            continue;
        }
        if let Err(error) = validate_sse_response(&response, "MCP listening stream GET") {
            tracing::warn!(error = %error, "MCP listening stream returned non-SSE response; backing off");
            sleep_with_backoff(&mut backoff_ms).await;
            continue;
        }

        // Successful 200 with SSE body. Reset backoff and consume.
        backoff_ms = BACKOFF_BASE_MS;
        let stream_session = session_snapshot.snapshot();
        let outcome =
            consume_listening_stream(&ctx, response, &stream_session, &mut last_event_id).await;
        if !ctx.alive.load(Ordering::SeqCst) {
            break;
        }
        if let Some(delay) = outcome.retry_delay {
            tokio::time::sleep(delay).await;
        } else {
            sleep_with_backoff(&mut backoff_ms).await;
        }
    }
}

async fn sleep_with_backoff(backoff_ms: &mut u64) {
    tokio::time::sleep(Duration::from_millis(*backoff_ms)).await;
    *backoff_ms = (*backoff_ms * 2).min(30_000);
}

/// Drain the SSE response body, forwarding each event to the
/// appropriate channel. Returns when the stream ends (clean close or
/// IO error). The caller loops to reconnect.
#[derive(Debug, Default)]
struct ListeningStreamOutcome {
    retry_delay: Option<Duration>,
}

async fn consume_listening_stream(
    ctx: &HttpListenerCtx,
    response: reqwest::Response,
    stream_session: &HttpSessionSnapshot,
    last_event_id: &mut Option<String>,
) -> ListeningStreamOutcome {
    let mut outcome = ListeningStreamOutcome::default();
    let mut event_lines: Vec<String> = Vec::new();
    let mut event_bytes = 0usize;
    let mut line_buf: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        if !ctx.alive.load(Ordering::SeqCst) {
            return outcome;
        }
        let chunk = match chunk {
            Ok(c) => c,
            Err(err) => {
                tracing::debug!(error = %err, "MCP listening stream chunk error");
                return outcome;
            }
        };
        for byte in chunk {
            if byte == b'\n' {
                let mut line = match String::from_utf8(std::mem::take(&mut line_buf)) {
                    Ok(s) => s,
                    Err(err) => {
                        tracing::warn!(error = %err, "MCP listening stream invalid UTF-8");
                        return outcome;
                    }
                };
                if line.ends_with('\r') {
                    line.pop();
                }
                if line.is_empty() {
                    apply_event_id_update(last_event_id, extract_event_id(&event_lines));
                    match bounded_sse_retry_delay(&event_lines, "MCP listening stream") {
                        Ok(Some(delay)) => outcome.retry_delay = Some(delay),
                        Ok(None) => {}
                        Err(err) => {
                            tracing::warn!(error = %err, "MCP listening stream rejected SSE retry delay");
                            return outcome;
                        }
                    }
                    process_listening_event(ctx, stream_session, &event_lines).await;
                    event_lines.clear();
                    event_bytes = 0;
                } else if let Err(err) = push_sse_event_line(
                    &mut event_lines,
                    &mut event_bytes,
                    line,
                    "MCP listening stream",
                ) {
                    tracing::warn!(error = %err, "MCP listening stream rejected oversized SSE event");
                    return outcome;
                }
            } else if let Err(err) = push_sse_line_byte(&mut line_buf, byte, "MCP listening stream")
            {
                tracing::warn!(error = %err, "MCP listening stream rejected oversized SSE line");
                return outcome;
            }
        }
    }
    // Final unterminated event (no trailing blank line).
    if !line_buf.is_empty() {
        let mut line = match String::from_utf8(std::mem::take(&mut line_buf)) {
            Ok(line) => line,
            Err(err) => {
                tracing::warn!(error = %err, "MCP listening stream invalid UTF-8");
                return outcome;
            }
        };
        if line.ends_with('\r') {
            line.pop();
        }
        if let Err(err) = push_sse_event_line(
            &mut event_lines,
            &mut event_bytes,
            line,
            "MCP listening stream",
        ) {
            tracing::warn!(error = %err, "MCP listening stream rejected oversized SSE event");
            return outcome;
        }
    }
    if !event_lines.is_empty() {
        apply_event_id_update(last_event_id, extract_event_id(&event_lines));
        match bounded_sse_retry_delay(&event_lines, "MCP listening stream") {
            Ok(Some(delay)) => outcome.retry_delay = Some(delay),
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(error = %err, "MCP listening stream rejected SSE retry delay");
                return outcome;
            }
        }
        process_listening_event(ctx, stream_session, &event_lines).await;
    }
    outcome
}

async fn process_listening_event(
    ctx: &HttpListenerCtx,
    stream_session: &HttpSessionSnapshot,
    event_lines: &[String],
) {
    if ctx.session.read().await.generation != stream_session.generation {
        // This event belongs to a GET stream opened under an expired
        // HTTP session. Do not apply stale notifications locally and,
        // most importantly, do not answer server requests using a newer
        // session id.
        tracing::debug!(
            stream_generation = stream_session.generation,
            "dropping MCP listening event from stale HTTP session generation"
        );
        return;
    }

    let payload = match sse_data_payload(event_lines, "MCP listening stream") {
        Ok(Some(payload)) => payload,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(error = %err, "MCP listening stream rejected oversized SSE payload");
            return;
        }
    };
    let value = match serde_json::from_str::<Value>(&payload) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, payload, "MCP listening stream invalid JSON");
            return;
        }
    };
    let message = match parse_json_rpc_message(value) {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!(error = %err, "MCP listening stream message parse failed");
            return;
        }
    };

    match message {
        JsonRpcMessage::Notification(notification) => {
            if let Some(kind) = list_changed_method(&notification.method) {
                forward_list_changed(&ctx.list_changed_tx, kind);
            } else if let Some(uri) = resource_updated_uri(&notification) {
                let _ = ctx.resource_updated_tx.try_send(uri);
            }
            // progress notifications without a request_id can't be
            // routed; drop. The listening stream isn't tied to a
            // single client request.
        }
        JsonRpcMessage::Request(request) => {
            // Server-initiated request on the GET listening stream.
            // Per spec 2025-11-25 §Streamable HTTP / Listening for
            // Messages from the Server, these messages SHOULD be
            // unrelated to concurrently-running client requests — so
            // we MUST NOT use per-call-cardinality routing here.
            // Doing so would let a sampling/createMessage on this
            // stream be answered by an arbitrary in-flight agent's
            // executor (the cross-agent leak the per-call factory
            // exists to prevent).
            //
            // Resolution: use the registry-level fallback handler
            // only. If no fallback is configured, the request is
            // rejected with method-not-supported (-32601). Servers
            // wanting per-agent sampling must initiate the request
            // on the per-request SSE stream where request_id-based
            // routing applies.
            let chosen = ctx.fallback_sampling.clone();
            let response =
                handle_server_request(chosen.as_deref(), ctx.roots.as_slice(), &request).await;
            // POST the response back to the server. See
            // post_listener_response for status handling.
            let _ = post_listener_response(ctx, stream_session, response).await;
        }
        JsonRpcMessage::Response(_) => {
            // Responses on the listening stream are unexpected — the
            // listening stream is for server-initiated traffic.
            // Ignore.
        }
    }
}

async fn post_listener_response(
    ctx: &HttpListenerCtx,
    stream_session: &HttpSessionSnapshot,
    response: JsonRpcResponse,
) -> Result<(), McpTransportError> {
    let mut req = ctx.client.post(&ctx.endpoint).header(
        reqwest::header::ACCEPT,
        "application/json, text/event-stream",
    );
    let current_session = ctx.session.read().await.snapshot();
    if current_session.generation != stream_session.generation {
        tracing::debug!(
            stream_generation = stream_session.generation,
            current_generation = current_session.generation,
            "dropping MCP listening response for stale HTTP session generation"
        );
        return Err(McpTransportError::ProtocolError(
            MCP_SESSION_EXPIRED.to_string(),
        ));
    }
    let sent_session_id = stream_session.session_id.clone();
    let sent_protocol_version = stream_session.protocol_version.clone();
    if sent_protocol_version.is_none() {
        return Err(McpTransportError::ProtocolError(
            MCP_SESSION_EXPIRED.to_string(),
        ));
    }
    if let Some(ref pv) = sent_protocol_version {
        req = req.header("MCP-Protocol-Version", pv.clone());
    }
    if let Some(ref id) = sent_session_id {
        req = req.header("MCP-Session-Id", id.clone());
    }
    let http_response = req.json(&response).send().await.map_err(|e| {
        McpTransportError::TransportError(format!("listener response POST failed: {}", e))
    })?;
    // Per spec 2025-11-25 §Streamable HTTP: a client POSTing a
    // JSON-RPC response/notification should expect 202 Accepted (or
    // 200/204 for some servers). 4xx/5xx means the server rejected
    // our response to its server-initiated request — silently
    // dropping that would leave the server waiting indefinitely.
    // Log at warn so oncall can see it; the listener task continues.
    let status = http_response.status();
    if status == reqwest::StatusCode::NOT_FOUND && sent_session_id.is_some() {
        tracing::debug!(
            "listener response POST got 404 — resetting session + capabilities to force re-handshake"
        );
        reset_http_session_state_if_current(
            &ctx.session,
            &ctx.capabilities,
            sent_session_id.as_deref(),
            sent_protocol_version.as_deref(),
            stream_session.generation,
        )
        .await;
        return Ok(());
    }
    if !status.is_success() && status != reqwest::StatusCode::NO_CONTENT {
        tracing::warn!(
            %status,
            "listener response POST returned non-success status; server may treat its request as unanswered"
        );
    }
    Ok(())
}

#[async_trait]
impl McpToolTransport for ProgressAwareHttpTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        let result = self
            .send_initialized_request("tools/list", Some(json!({})), None)
            .await?;
        let list_result: ListToolsResult = serde_json::from_value(result)?;
        Ok(list_result.tools)
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
        let result = self
            .send_initialized_request("prompts/list", Some(json!({})), None)
            .await?;
        let list_result: ListPromptsResult = serde_json::from_value(result)?;
        Ok(list_result.prompts)
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<McpPromptResult, McpTransportError> {
        let result = self
            .send_initialized_request(
                "prompts/get",
                Some(json!({
                    "name": name,
                    "arguments": arguments,
                })),
                None,
            )
            .await?;
        serde_json::from_value(result).map_err(Into::into)
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
        let result = self
            .send_initialized_request("resources/list", Some(json!({})), None)
            .await?;
        let list_result: ListResourcesResult = serde_json::from_value(result)?;
        Ok(list_result.resources)
    }

    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
        context: McpCallContext,
    ) -> Result<CallToolResult, McpTransportError> {
        let McpCallContext {
            metadata,
            cancellation,
            sampling,
        } = context;

        // Pre-check cancellation BEFORE anything observable: any side
        // effect we trigger past this point (id allocation, sampling
        // map insertion, HTTP initialize) costs the server some work
        // we'd need to unwind. Caller already cancelled → error out
        // immediately.
        if let Some(ref token) = cancellation
            && token.is_cancelled()
        {
            return Err(McpTransportError::TransportError(
                CANCELLED_BY_CLIENT.to_string(),
            ));
        }

        let (progress_token, progress_sender) = match progress_tx {
            Some(sender) => {
                let token =
                    ProgressToken::Number(self.next_progress_token.fetch_add(1, Ordering::SeqCst));
                let key = ProgressTokenKey::from(&token);
                (Some(token), Some((key, sender)))
            }
            None => (None, None),
        };

        let meta = build_call_tool_meta(progress_token, &metadata)?;

        let params = CallToolParams {
            name: name.to_string(),
            arguments: Some(args),
            task: None,
            meta,
        };
        let params_value = serde_json::to_value(&params)?;

        // Initialize runs UNINTERRUPTED — racing it with cancellation
        // can leave the session half-constructed: server assigns a
        // session id, we drop the future before reading it, and the
        // orphaned server-side session lingers. Initialize is shared
        // across all callers of this transport (cached in
        // `capabilities`) — local to this call, it's a setup step, not
        // the cancellable work.
        self.initialize_if_needed().await?;

        // Re-check cancellation after initialize completed (it may
        // have taken non-trivial time). Tools/call hasn't been
        // allocated/sent yet, so this is still a clean exit.
        if let Some(ref token) = cancellation
            && token.is_cancelled()
        {
            return Err(McpTransportError::TransportError(
                CANCELLED_BY_CLIENT.to_string(),
            ));
        }

        // Retry loop for the spec-defined `MCP session expired` case
        // before the server accepted the request (the initial POST
        // returned 404 against a request bearing our session id — see
        // `post_message`). Once a per-request SSE body has begun, the
        // tool may already be executing; resume failures are mapped to
        // `MCP_SESSION_EXPIRED_AFTER_ACCEPT` and MUST NOT silently
        // replay a potentially side-effectful `tools/call`.
        // One pre-accept retry is enough: server told us the session is
        // gone, so we reset, reinitialize, and resend with a fresh id.
        // Anything beyond that is almost certainly a genuine
        // server-side failure, not session staleness.
        let mut session_retried = false;
        let result_value = loop {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);

            // Register the per-call sampling decision deterministically
            // BEFORE the request hits the wire. Awaiting the lock guarantees
            // the reader sees this entry before it can receive a
            // `sampling/createMessage` for our request id on the per-request
            // SSE stream.
            let mut handler_guard = PerCallSamplingGuard::register(
                Arc::clone(&self.per_call_sampling),
                id,
                sampling.clone(),
            )
            .await;

            let request_sent = Arc::new(AtomicBool::new(false));
            let request_session = Arc::new(tokio::sync::Mutex::new(None::<HttpSessionSnapshot>));
            let request_fut = self.send_request_with_id(
                id,
                "tools/call",
                Some(params_value.clone()),
                progress_sender.clone(),
                Some(Arc::clone(&request_sent)),
                Some(Arc::clone(&request_session)),
            );

            let attempt_result = match cancellation.as_ref() {
                None => request_fut.await,
                Some(token) => {
                    tokio::pin!(request_fut);
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => {
                            // Per spec (2025-11-25 §Cancellation): SHOULD
                            // emit notifications/cancelled with the
                            // in-flight requestId so the server stops
                            // processing. For HTTP this flag means the
                            // request is believed issued/in-progress; the
                            // POST may still be awaiting response headers.
                            // Receivers should ignore unknown request ids.
                            if request_sent.load(Ordering::SeqCst) {
                                if let Some(session) = request_session.lock().await.clone() {
                                    match self
                                        .send_notification_with_session(
                                            "notifications/cancelled",
                                            Some(json!({
                                                "requestId": id,
                                                "reason": "client run cancelled",
                                            })),
                                            &session,
                                        )
                                        .await
                                    {
                                        Ok(()) => {}
                                        // 404 here means the original
                                        // session is already gone. The
                                        // captured-session POST keeps
                                        // this cancellation from leaking
                                        // into a newer session; the
                                        // in-flight old-session work may
                                        // still run to completion server-side.
                                        Err(McpTransportError::ProtocolError(ref msg))
                                            if msg == MCP_SESSION_EXPIRED =>
                                        {
                                            tracing::warn!(
                                                request_id = id,
                                                "session expired while sending notifications/cancelled"
                                            );
                                        }
                                        Err(err) => {
                                            tracing::warn!(
                                                error = %err,
                                                request_id = id,
                                                "notifications/cancelled send failed — server may continue executing the cancelled tool call"
                                            );
                                        }
                                    }
                                } else {
                                    tracing::warn!(
                                        request_id = id,
                                        "notifications/cancelled skipped because no HTTP session snapshot was captured"
                                    );
                                }
                            }
                            // Deterministic guard cleanup before
                            // returning so no request-bound sampling
                            // entry survives this call.
                            handler_guard.unregister().await;
                            return Err(McpTransportError::TransportError(
                                CANCELLED_BY_CLIENT.to_string(),
                            ));
                        }
                        result = &mut request_fut => result,
                    }
                }
            };

            match attempt_result {
                Ok(value) => {
                    // Success path — explicitly unregister so the
                    // sampling map is consistent before we return.
                    handler_guard.unregister().await;
                    break value;
                }
                Err(McpTransportError::ProtocolError(ref message))
                    if message == MCP_SESSION_EXPIRED && !session_retried =>
                {
                    // Drop the guard for the now-invalid id (await
                    // the async cleanup, not the sync Drop), then
                    // retry with the current session. The request path
                    // that observed the 404 already performed a
                    // compare-and-reset if it still owned the current
                    // session; a stale 404 must not clobber a newer
                    // session installed by another request.
                    handler_guard.unregister().await;
                    session_retried = true;
                    self.initialize_if_needed().await?;
                    if let Some(ref token) = cancellation
                        && token.is_cancelled()
                    {
                        return Err(McpTransportError::TransportError(
                            CANCELLED_BY_CLIENT.to_string(),
                        ));
                    }
                    continue;
                }
                Err(err) => {
                    handler_guard.unregister().await;
                    return Err(err);
                }
            }
        };

        serde_json::from_value(result_value).map_err(Into::into)
    }

    fn transport_type(&self) -> TransportTypeId {
        TransportTypeId::Http
    }

    async fn server_capabilities(&self) -> Result<Option<ServerCapabilities>, McpTransportError> {
        Ok(Some(self.initialize_if_needed().await?))
    }

    async fn take_list_changed_receiver(&self) -> Option<mpsc::Receiver<ListChangedKind>> {
        self.list_changed_rx.lock().await.take()
    }

    async fn take_resource_updated_receiver(&self) -> Option<mpsc::Receiver<String>> {
        self.resource_updated_rx.lock().await.take()
    }

    async fn subscribe_resource(&self, uri: &str) -> Result<(), McpTransportError> {
        self.send_initialized_request("resources/subscribe", Some(json!({ "uri": uri })), None)
            .await
            .map(|_| ())
    }

    async fn unsubscribe_resource(&self, uri: &str) -> Result<(), McpTransportError> {
        self.send_initialized_request("resources/unsubscribe", Some(json!({ "uri": uri })), None)
            .await
            .map(|_| ())
    }

    async fn complete(&self, params: CompleteParams) -> Result<CompleteResult, McpTransportError> {
        let result = self
            .send_initialized_request(
                "completion/complete",
                Some(serde_json::to_value(&params)?),
                None,
            )
            .await?;
        serde_json::from_value(result).map_err(Into::into)
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, McpTransportError> {
        self.send_initialized_request("resources/read", Some(json!({ "uri": uri })), None)
            .await
    }

    async fn close(&self) -> Result<(), McpTransportError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // Per spec (2025-11-25 §Streamable HTTP / Session Management):
        // "Clients that no longer need a particular session SHOULD send an
        //  HTTP DELETE to the MCP endpoint with the MCP-Session-Id header,
        //  to explicitly terminate the session. The server MAY respond
        //  to this request with HTTP 405 Method Not Allowed."
        //
        // Without this, the server-side session state lingers until its
        // own TTL fires — for long-running awaken processes that toggle
        // / reconnect MCP servers, this accumulates zombie sessions.
        //
        // Errors are swallowed (best-effort): the server may legitimately
        // 405 to refuse termination, or the network may be down. Either
        // way we still tear down local state.

        // Stop the background GET listener BEFORE tearing down the
        // session — otherwise the listener might race the DELETE and
        // re-establish a new stream against the about-to-be-killed
        // session. The atomic guards the consume loop's chunked
        // wakeups; the abort handle short-circuits any in-flight
        // request.
        self.listener_alive.store(false, Ordering::SeqCst);
        if let Ok(mut slot) = self.listener_abort.lock()
            && let Some(handle) = slot.take()
        {
            handle.abort();
        }

        let session_snapshot = self.session.read().await.snapshot();
        if let Some(session_id) = session_snapshot.session_id {
            self.send_session_termination(
                &session_id,
                session_snapshot.protocol_version.as_deref(),
            )
            .await;
        }

        {
            let mut session = self.session.write().await;
            session.session_id = None;
            session.protocol_version = None;
            session.started_at = None;
        }

        {
            let mut capabilities = self.capabilities.lock().await;
            *capabilities = None;
        }

        Ok(())
    }

    async fn current_session_started_at(&self) -> Option<SystemTime> {
        self.session.read().await.started_at
    }

    async fn current_session_generation(&self) -> Option<u64> {
        Some(self.session.read().await.generation)
    }
}

impl Drop for ProgressAwareHttpTransport {
    fn drop(&mut self) {
        // Cancel the background GET listener if it's still alive —
        // without this, the task would keep retrying connections
        // against an endpoint nobody is reading from. `close()`
        // already does this on the happy path; Drop is the backstop
        // for `Arc::drop` without an explicit close.
        self.listener_alive.store(false, Ordering::SeqCst);
        if let Ok(mut slot) = self.listener_abort.lock()
            && let Some(handle) = slot.take()
        {
            handle.abort();
        }
    }
}

impl ProgressAwareHttpTransport {
    /// Best-effort `DELETE <endpoint>` with the session id header so the
    /// server can immediately free session state. Swallows all errors —
    /// the client has already decided to terminate, so a server 405 / 5xx
    /// / network error doesn't change the outcome locally.
    async fn send_session_termination(&self, session_id: &str, protocol_version: Option<&str>) {
        let mut request = self
            .client
            .delete(&self.endpoint)
            .header("MCP-Session-Id", session_id.to_string());
        if let Some(protocol_version) = protocol_version {
            request = request.header("MCP-Protocol-Version", protocol_version.to_string());
        }
        match request.send().await {
            Ok(response) if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED => {
                tracing::debug!(
                    endpoint = %self.endpoint,
                    "MCP server refused DELETE-session (405); session will expire on server TTL"
                );
            }
            Ok(response) if !response.status().is_success() => {
                tracing::debug!(
                    endpoint = %self.endpoint,
                    status = %response.status(),
                    "MCP DELETE-session non-success; ignoring"
                );
            }
            Ok(_) => {}
            Err(err) => {
                tracing::debug!(
                    endpoint = %self.endpoint,
                    error = %err,
                    "MCP DELETE-session failed; ignoring"
                );
            }
        }
    }
}

// ── connect_transport ──

pub(crate) async fn connect_transport(
    config: &McpServerConnectionConfig,
    sampling_handler: Option<Arc<dyn SamplingHandler>>,
    roots: Arc<Vec<Root>>,
    advertise_sampling: bool,
) -> Result<Arc<dyn McpToolTransport>, McpTransportError> {
    match config.transport {
        TransportTypeId::Stdio => {
            let transport = ProgressAwareStdioTransport::connect(
                config,
                sampling_handler,
                roots,
                advertise_sampling,
            )
            .await?;
            Ok(Arc::new(transport))
        }
        TransportTypeId::Http => {
            let transport = ProgressAwareHttpTransport::connect(
                config,
                sampling_handler,
                roots,
                advertise_sampling,
            )?;
            Ok(Arc::new(transport))
        }
    }
}

// ── Shared helpers ──

fn initialize_params(capabilities: Value, config: Value) -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": capabilities,
        "clientInfo": serde_json::to_value(ClientInfo::new(
            "awaken-mcp",
            env!("CARGO_PKG_VERSION"),
        )).unwrap_or_else(|_| json!({})),
        "config": config,
    })
}

fn map_response_payload(payload: JsonRpcPayload) -> Result<Value, McpTransportError> {
    match payload {
        JsonRpcPayload::Success { result } => Ok(result),
        JsonRpcPayload::Error { error } => Err(McpTransportError::ServerError(format!(
            "MCP Error: {}",
            error
        ))),
    }
}

async fn handle_progress_notification(
    subscribers: &Arc<
        tokio::sync::Mutex<HashMap<ProgressTokenKey, mpsc::Sender<McpProgressUpdate>>>,
    >,
    notification: JsonRpcNotification,
) {
    let Some((key, update)) = decode_progress_notification(notification) else {
        return;
    };
    let sender = subscribers.lock().await.get(&key).cloned();
    if let Some(sender) = sender {
        match sender.try_send(update) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                subscribers.lock().await.remove(&key);
            }
        }
    }
}

pub(crate) fn decode_progress_notification(
    notification: JsonRpcNotification,
) -> Option<(ProgressTokenKey, McpProgressUpdate)> {
    if notification.method != "notifications/progress" {
        return None;
    }
    let params = notification.params?;
    let params = serde_json::from_value::<ProgressNotificationParams>(params).ok()?;
    let key = ProgressTokenKey::from(&params.progress_token);
    let update = McpProgressUpdate {
        progress: params.progress,
        total: params.total,
        message: params.message,
    };
    Some((key, update))
}

pub(crate) fn tool_result_error_text(result: &CallToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        return text;
    }
    if let Some(structured) = result.structured_content.clone() {
        return structured.to_string();
    }
    if !result.content.is_empty() {
        return serde_json::to_string(&result.content)
            .unwrap_or_else(|_| "Unknown error".to_string());
    }
    "Unknown error".to_string()
}

fn parse_json_rpc_message(value: Value) -> Result<JsonRpcMessage, McpTransportError> {
    match serde_json::from_value::<JsonRpcMessage>(value.clone()) {
        Ok(message) => Ok(message),
        Err(_) => serde_json::from_value::<JsonRpcResponse>(value)
            .map(JsonRpcMessage::Response)
            .map_err(McpTransportError::from),
    }
}

pub(crate) fn decode_http_response_payload(
    body: Value,
    request_id: i64,
    progress_registration: Option<(ProgressTokenKey, mpsc::Sender<McpProgressUpdate>)>,
) -> Result<Value, McpTransportError> {
    let progress_key = progress_registration.as_ref().map(|(key, _)| key.clone());
    let progress_tx = progress_registration
        .as_ref()
        .map(|(_, sender)| sender.clone());
    let mut matched_response: Option<Result<Value, McpTransportError>> = None;

    let mut process_message = |message: JsonRpcMessage| match message {
        JsonRpcMessage::Response(response) => {
            if matches!(response.id, JsonRpcId::Number(id) if id == request_id) {
                matched_response = Some(map_response_payload(response.payload));
            }
        }
        JsonRpcMessage::Notification(notification) => {
            let Some(expected_key) = progress_key.as_ref() else {
                return;
            };
            let Some(sender) = progress_tx.as_ref() else {
                return;
            };
            let Some((key, update)) = decode_progress_notification(notification) else {
                return;
            };
            if key == *expected_key {
                let _ = sender.try_send(update);
            }
        }
        JsonRpcMessage::Request(_) => {}
    };

    match body {
        Value::Array(items) => {
            for item in items {
                let message = parse_json_rpc_message(item)?;
                process_message(message);
            }
        }
        other => {
            let message = parse_json_rpc_message(other)?;
            process_message(message);
        }
    }

    matched_response.unwrap_or_else(|| {
        Err(McpTransportError::ProtocolError(format!(
            "Missing response for request id {}",
            request_id
        )))
    })
}

/// Resolve which sampling handler should service an incoming
/// server-initiated request. Per-call handlers (registered by `call_tool`
/// in flight) take precedence over the transport-level fallback when
/// there is **exactly one** call in flight — that's the unambiguous
/// case where we know which agent's executor should service the
/// sampling request. With zero or multiple in-flight calls we cannot
/// route safely (the MCP spec gives no correlation between
/// `sampling/createMessage` and a specific `tools/call` id), so we fall
/// back to the transport's fixed handler. Operators who need stricter
/// per-call routing in the >1-in-flight case can serialize their tool
/// calls or contribute server-side echoing of `params._meta.awaken/in_response_to_call_id`.
/// Stdio routing: server-initiated `sampling/createMessage` from a
/// stdio server has no spec-mandated correlation id back to a specific
/// in-flight `tools/call`. We therefore never use per-call/per-agent
/// handlers on stdio; only a transport-level fixed fallback can answer
/// these requests.
fn select_stdio_sampling_handler(
    fallback: Option<&Arc<dyn SamplingHandler>>,
) -> Option<Arc<dyn SamplingHandler>> {
    fallback.cloned()
}

pub(crate) async fn handle_server_request(
    sampling_handler: Option<&dyn SamplingHandler>,
    roots: &[Root],
    request: &JsonRpcRequest,
) -> JsonRpcResponse {
    match request.method.as_str() {
        "sampling/createMessage" => {
            let Some(handler) = sampling_handler else {
                return JsonRpcResponse::error(
                    request.id.clone(),
                    -32601,
                    "Sampling not supported by this client".to_string(),
                    None,
                );
            };
            let params = match request
                .params
                .as_ref()
                .and_then(|p| serde_json::from_value::<CreateMessageParams>(p.clone()).ok())
            {
                Some(p) => p,
                None => {
                    return JsonRpcResponse::error(
                        request.id.clone(),
                        -32602,
                        "Invalid sampling/createMessage params".to_string(),
                        None,
                    );
                }
            };
            match handler.handle_create_message(params).await {
                Ok(result) => {
                    let result_value = serde_json::to_value(&result).unwrap_or(Value::Null);
                    JsonRpcResponse::success(request.id.clone(), result_value)
                }
                Err(e) => JsonRpcResponse::error(request.id.clone(), -32000, e.to_string(), None),
            }
        }
        // Spec 2025-11-25 §Client features / Roots: server may call
        // `roots/list` only if the client advertised the `roots`
        // capability during initialize. We advertise based on whether
        // the roots vec was non-empty at construction, so an empty
        // slice here reflects either a non-advertising client or a
        // race during disable — return method-not-supported, NOT an
        // empty list, so a misbehaving server learns it asked for
        // something we never agreed to.
        "roots/list" => {
            if roots.is_empty() {
                return JsonRpcResponse::error(
                    request.id.clone(),
                    -32601,
                    "roots/list not supported (client did not advertise roots capability)"
                        .to_string(),
                    None,
                );
            }
            let result = ListRootsResult {
                roots: roots.to_vec(),
            };
            let result_value = serde_json::to_value(&result).unwrap_or(Value::Null);
            JsonRpcResponse::success(request.id.clone(), result_value)
        }
        _ => JsonRpcResponse::error(
            request.id.clone(),
            -32601,
            format!("Method not supported: {}", request.method),
            None,
        ),
    }
}

/// Extract plain text from MCP tool content items.
pub(crate) fn plain_text_content(content: &[ToolContent]) -> Option<String> {
    let mut text_parts = Vec::with_capacity(content.len());
    for item in content {
        match item {
            ToolContent::Text {
                text,
                annotations: None,
                meta: None,
            } => text_parts.push(text.as_str()),
            _ => return None,
        }
    }
    Some(text_parts.join("\n"))
}

/// Convert a CallToolResult to a Value suitable for awaken ToolResult data.
pub(crate) fn call_result_to_tool_data(call_result: &CallToolResult) -> Value {
    if call_result.structured_content.is_none()
        && let Some(text) = plain_text_content(&call_result.content)
    {
        return Value::String(text);
    }

    serde_json::to_value(call_result).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use super::*;
    use mcp::CreateMessageResult;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone, Debug)]
    struct UnitHttpRequest {
        method: String,
        headers: HashMap<String, String>,
        body: Value,
    }

    fn unit_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
    }

    fn unit_content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0)
    }

    async fn read_unit_http_request(stream: &mut tokio::net::TcpStream) -> Option<UnitHttpRequest> {
        let mut buf = Vec::new();
        let mut chunk = [0_u8; 1024];
        let (header_end_pos, body_len) = loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            let Some(end) = unit_header_end(&buf) else {
                continue;
            };
            let headers = std::str::from_utf8(&buf[..end]).ok()?;
            break (end, unit_content_length(headers));
        };

        while buf.len() < header_end_pos + body_len {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }

        let headers_text = std::str::from_utf8(&buf[..header_end_pos]).ok()?;
        let method = headers_text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().next())
            .unwrap_or_default()
            .to_string();
        let headers = headers_text
            .lines()
            .skip(1)
            .filter_map(|line| {
                let (key, value) = line.split_once(':')?;
                Some((key.trim().to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect();
        let body = if body_len == 0 {
            Value::Null
        } else {
            serde_json::from_slice(&buf[header_end_pos..header_end_pos + body_len]).ok()?
        };
        Some(UnitHttpRequest {
            method,
            headers,
            body,
        })
    }

    async fn write_unit_http_response(
        stream: &mut tokio::net::TcpStream,
        status: u16,
        content_type: &str,
        body: String,
        headers: &[(&str, String)],
    ) {
        let status_text = match status {
            200 => "OK",
            202 => "Accepted",
            400 => "Bad Request",
            _ => "OK",
        };
        let mut head = format!(
            "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (key, value) in headers {
            head.push_str(&format!("{key}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(body.as_bytes()).await;
        let _ = stream.shutdown().await;
    }

    // ── SSE event id extraction (R7 #6 Last-Event-ID resumability) ──

    #[test]
    fn extract_event_id_reads_id_line() {
        let event = vec!["id: 42".to_string(), "data: {}".to_string()];
        assert_eq!(extract_event_id(&event), SseEventIdUpdate::Set("42".into()));
    }

    #[test]
    fn extract_event_id_tolerates_no_space_after_colon() {
        // SSE spec permits `id:42` (no space).
        let event = vec!["id:99".to_string(), "data: {}".to_string()];
        assert_eq!(extract_event_id(&event), SseEventIdUpdate::Set("99".into()));
    }

    #[test]
    fn extract_event_id_returns_none_when_absent() {
        let event = vec!["data: {}".to_string(), "event: message".to_string()];
        assert_eq!(extract_event_id(&event), SseEventIdUpdate::Absent);
    }

    #[test]
    fn extract_event_id_treats_empty_value_as_reset() {
        // Per spec: `id:\n` resets the last-event-id. This is distinct
        // from an absent id so the reconnect path can clear stale state.
        let event = vec!["id: ".to_string(), "data: {}".to_string()];
        assert_eq!(extract_event_id(&event), SseEventIdUpdate::Reset);
    }

    #[test]
    fn extract_event_id_takes_last_id_when_repeated() {
        // SSE spec: the LAST id field in an event is authoritative.
        // Our impl scans from the end and returns the first match.
        let event = vec![
            "id: 1".to_string(),
            "data: ignored".to_string(),
            "id: 2".to_string(),
            "data: kept".to_string(),
        ];
        assert_eq!(extract_event_id(&event), SseEventIdUpdate::Set("2".into()));
    }

    #[test]
    fn apply_event_id_update_clears_existing_id_on_reset() {
        let mut last_event_id = Some("stale".to_string());
        apply_event_id_update(&mut last_event_id, SseEventIdUpdate::Reset);
        assert_eq!(last_event_id, None);
    }

    #[test]
    fn extract_retry_delay_reads_milliseconds() {
        let event = vec!["retry: 25".to_string(), "data: {}".to_string()];
        assert_eq!(extract_retry_delay(&event), Some(Duration::from_millis(25)));
    }

    #[test]
    fn extract_retry_delay_ignores_invalid_value() {
        let event = vec!["retry: soon".to_string(), "data: {}".to_string()];
        assert_eq!(extract_retry_delay(&event), None);
    }

    #[test]
    fn extract_retry_delay_keeps_latest_valid_value() {
        let event = vec![
            "retry: 10".to_string(),
            "retry: 20".to_string(),
            "retry: soon".to_string(),
            "data: {}".to_string(),
        ];
        assert_eq!(extract_retry_delay(&event), Some(Duration::from_millis(20)));
    }

    #[test]
    fn bounded_sse_retry_delay_rejects_unbounded_sleep() {
        let event = vec!["retry: 86400000".to_string(), "data:".to_string()];
        let err = bounded_sse_retry_delay(&event, "test stream")
            .expect_err("huge retry delay must be rejected");
        assert!(
            format!("{err}").contains("SSE retry delay exceeded"),
            "unexpected error: {err}"
        );
    }

    // ── notifications/resources/updated parsing ──

    #[test]
    fn resource_updated_uri_extracts_uri() {
        let notification = JsonRpcNotification::new(
            "notifications/resources/updated",
            Some(json!({ "uri": "file:///tmp/notes.md" })),
        );
        assert_eq!(
            resource_updated_uri(&notification),
            Some("file:///tmp/notes.md".to_string())
        );
    }

    #[test]
    fn resource_updated_uri_ignores_other_methods() {
        let notification = JsonRpcNotification::new(
            "notifications/tools/list_changed",
            Some(json!({ "uri": "file:///irrelevant" })),
        );
        assert_eq!(resource_updated_uri(&notification), None);
    }

    #[test]
    fn resource_updated_uri_rejects_missing_or_non_string_uri() {
        // Defensive: malformed notifications shouldn't crash the
        // reader; just yield None so they're ignored.
        let no_params = JsonRpcNotification::new("notifications/resources/updated", None);
        assert_eq!(resource_updated_uri(&no_params), None);

        let int_uri = JsonRpcNotification::new(
            "notifications/resources/updated",
            Some(json!({ "uri": 42 })),
        );
        assert_eq!(resource_updated_uri(&int_uri), None);

        let no_uri_field = JsonRpcNotification::new(
            "notifications/resources/updated",
            Some(json!({ "other": "x" })),
        );
        assert_eq!(resource_updated_uri(&no_uri_field), None);
    }

    // ── list_changed notification parsing ──

    #[test]
    fn list_changed_method_recognises_three_kinds() {
        assert_eq!(
            list_changed_method("notifications/tools/list_changed"),
            Some(ListChangedKind::Tools)
        );
        assert_eq!(
            list_changed_method("notifications/prompts/list_changed"),
            Some(ListChangedKind::Prompts)
        );
        assert_eq!(
            list_changed_method("notifications/resources/list_changed"),
            Some(ListChangedKind::Resources)
        );
    }

    #[test]
    fn list_changed_method_ignores_other_methods() {
        // Strict match: nothing else (progress, cancelled, custom)
        // should be misclassified as list_changed.
        assert_eq!(list_changed_method("notifications/progress"), None);
        assert_eq!(list_changed_method("notifications/cancelled"), None);
        assert_eq!(list_changed_method("notifications/initialized"), None);
        assert_eq!(list_changed_method("notifications/tools/listChanged"), None); // wrong casing
        assert_eq!(list_changed_method(""), None);
        assert_eq!(list_changed_method("tools/list_changed"), None); // missing prefix
    }

    #[test]
    fn list_changed_forwarder_prioritizes_cached_tools_catalogue() {
        let (tx, mut rx) = mpsc::channel(LIST_CHANGED_CHANNEL_CAPACITY);
        for _ in 0..(LIST_CHANGED_CHANNEL_CAPACITY * 2) {
            forward_list_changed(&tx, ListChangedKind::Prompts);
            forward_list_changed(&tx, ListChangedKind::Resources);
        }
        forward_list_changed(&tx, ListChangedKind::Tools);

        assert_eq!(rx.try_recv(), Ok(ListChangedKind::Tools));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn list_changed_forwarder_bounds_duplicate_tools_notifications() {
        let (tx, mut rx) = mpsc::channel(LIST_CHANGED_CHANNEL_CAPACITY);
        for _ in 0..(LIST_CHANGED_CHANNEL_CAPACITY * 2) {
            forward_list_changed(&tx, ListChangedKind::Tools);
        }

        assert_eq!(rx.len(), LIST_CHANGED_CHANNEL_CAPACITY);
        let mut drained = 0usize;
        while rx.try_recv().is_ok() {
            drained += 1;
        }
        assert_eq!(drained, LIST_CHANGED_CHANNEL_CAPACITY);
    }

    // ── protocol-version negotiation ──

    #[test]
    fn negotiate_protocol_version_accepts_current_spec() {
        // The version awaken sends on the wire must always be in the
        // supported list — otherwise initialize would always reject
        // ourselves.
        let negotiated =
            negotiate_protocol_version(MCP_PROTOCOL_VERSION).expect("current version is supported");
        assert_eq!(negotiated, MCP_PROTOCOL_VERSION);
        assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&MCP_PROTOCOL_VERSION));
    }

    #[test]
    fn negotiate_protocol_version_rejects_unknown() {
        // Server replies with a version older than anything we know we
        // can wire-compat-talk on. Hard reject; the alternative is
        // sending `params._meta` (added in 2025-06-18) or
        // `MCP-Session-Id` semantics that older servers can't parse.
        let err = negotiate_protocol_version("2000-01-01").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("2000-01-01"),
            "error names the version we got: {msg}"
        );
        assert!(
            msg.contains(MCP_PROTOCOL_VERSION),
            "error names the version we expect: {msg}"
        );
    }

    #[test]
    fn negotiate_protocol_version_rejects_empty() {
        // Defensive: a misbehaving server could echo back "" — should
        // surface as a clear handshake error rather than be silently
        // accepted.
        assert!(negotiate_protocol_version("").is_err());
    }

    // ── handle_server_request tests ──

    struct MockSamplingHandler {
        response_text: String,
    }

    #[async_trait]
    impl SamplingHandler for MockSamplingHandler {
        async fn handle_create_message(
            &self,
            _params: CreateMessageParams,
        ) -> Result<CreateMessageResult, McpTransportError> {
            use mcp::{Role, SamplingContent};
            Ok(CreateMessageResult {
                role: Role::Assistant,
                content: vec![SamplingContent::Text {
                    text: self.response_text.clone(),
                    annotations: None,
                    meta: None,
                }],
                model: "mock-model".to_string(),
                stop_reason: Some("end_turn".to_string()),
                meta: None,
            })
        }
    }

    struct FailingSamplingHandler;

    #[async_trait]
    impl SamplingHandler for FailingSamplingHandler {
        async fn handle_create_message(
            &self,
            _params: CreateMessageParams,
        ) -> Result<CreateMessageResult, McpTransportError> {
            Err(McpTransportError::TransportError(
                "handler failed".to_string(),
            ))
        }
    }

    fn sampling_request(id: i64, params: Value) -> JsonRpcRequest {
        JsonRpcRequest::new(
            JsonRpcId::Number(id),
            "sampling/createMessage".to_string(),
            Some(params),
        )
    }

    #[tokio::test]
    async fn handle_sampling_request_with_handler_succeeds() {
        let handler = MockSamplingHandler {
            response_text: "I can help".to_string(),
        };
        let request = sampling_request(
            1,
            json!({
                "messages": [],
                "maxTokens": 100,
            }),
        );
        let response = handle_server_request(Some(&handler), &[], &request).await;
        match response.payload {
            mcp::JsonRpcPayload::Success { result } => {
                assert_eq!(result["model"], json!("mock-model"));
                assert_eq!(result["content"][0]["text"], json!("I can help"));
            }
            mcp::JsonRpcPayload::Error { error } => {
                panic!("expected success, got error: {}", error);
            }
        }
    }

    #[tokio::test]
    async fn handle_sampling_request_without_handler_returns_error() {
        let request = sampling_request(
            2,
            json!({
                "messages": [],
                "maxTokens": 100,
            }),
        );
        let response = handle_server_request(None, &[], &request).await;
        match response.payload {
            mcp::JsonRpcPayload::Error { error } => {
                assert!(error.to_string().contains("Sampling not supported"));
            }
            _ => panic!("expected error response"),
        }
    }

    #[tokio::test]
    async fn handle_sampling_request_with_invalid_params_returns_error() {
        let handler = MockSamplingHandler {
            response_text: "unused".to_string(),
        };
        let request = sampling_request(3, json!({"invalid": true}));
        let response = handle_server_request(Some(&handler), &[], &request).await;
        match response.payload {
            mcp::JsonRpcPayload::Error { error } => {
                assert!(error.to_string().contains("Invalid sampling/createMessage"));
            }
            _ => panic!("expected error response"),
        }
    }

    #[tokio::test]
    async fn handle_sampling_request_handler_error_propagates() {
        let handler = FailingSamplingHandler;
        let request = sampling_request(
            4,
            json!({
                "messages": [],
                "maxTokens": 100,
            }),
        );
        let response = handle_server_request(Some(&handler), &[], &request).await;
        match response.payload {
            mcp::JsonRpcPayload::Error { error } => {
                assert!(error.to_string().contains("handler failed"));
            }
            _ => panic!("expected error response"),
        }
    }

    #[tokio::test]
    async fn handle_unknown_method_returns_method_not_found() {
        let request = JsonRpcRequest::new(
            JsonRpcId::Number(5),
            "unknown/method".to_string(),
            Some(json!({})),
        );
        let response = handle_server_request(None, &[], &request).await;
        match response.payload {
            mcp::JsonRpcPayload::Error { error } => {
                assert!(error.to_string().contains("Method not supported"));
                assert!(error.to_string().contains("unknown/method"));
            }
            _ => panic!("expected error response"),
        }
    }

    // ── roots/list handling (R7 #3) ──

    #[tokio::test]
    async fn handle_roots_list_returns_configured_roots() {
        let roots = vec![
            Root {
                uri: "file:///home/user/project".to_string(),
                name: Some("project".to_string()),
                meta: None,
            },
            Root {
                uri: "file:///tmp/scratch".to_string(),
                name: None,
                meta: None,
            },
        ];
        let request = JsonRpcRequest::new(
            JsonRpcId::Number(10),
            "roots/list".to_string(),
            Some(json!({})),
        );
        let response = handle_server_request(None, &roots, &request).await;
        match response.payload {
            mcp::JsonRpcPayload::Success { result } => {
                let parsed: ListRootsResult =
                    serde_json::from_value(result).expect("ListRootsResult parses");
                assert_eq!(parsed.roots.len(), 2);
                assert_eq!(parsed.roots[0].uri, "file:///home/user/project");
                assert_eq!(parsed.roots[0].name.as_deref(), Some("project"));
                assert_eq!(parsed.roots[1].uri, "file:///tmp/scratch");
                assert!(parsed.roots[1].name.is_none());
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_roots_list_rejects_when_empty() {
        // Empty roots = capability was not advertised at initialize.
        // A misbehaving server still calling `roots/list` MUST see
        // method-not-supported (not an empty result), so it can't
        // continue assuming the client agreed to participate.
        let request = JsonRpcRequest::new(
            JsonRpcId::Number(11),
            "roots/list".to_string(),
            Some(json!({})),
        );
        let response = handle_server_request(None, &[], &request).await;
        match response.payload {
            mcp::JsonRpcPayload::Error { error } => {
                let msg = error.to_string();
                assert!(
                    msg.contains("roots/list not supported"),
                    "expected not-supported error, got: {msg}"
                );
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn decode_http_response_requires_matching_response_id() {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {"content": [{"type": "text", "text": "ok"}]}
        });
        let err = decode_http_response_payload(body, 1, None).expect_err("error");
        assert!(matches!(err, McpTransportError::ProtocolError(_)));
    }

    #[test]
    fn decode_http_batch_ignores_malformed_notifications() {
        let (tx, mut rx) = mpsc::channel(MCP_PROGRESS_CHANNEL_CAPACITY);
        let body = json!([
            { "jsonrpc": "2.0", "method": "notifications/progress" },
            { "jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": {"bad": true}, "progress": "oops"} },
            { "jsonrpc": "2.0", "method": "notifications/other", "params": {"x":1} },
            { "jsonrpc": "2.0", "id": 5, "result": {"content": [{"type":"text","text":"ok"}]} }
        ]);

        let result = decode_http_response_payload(body, 5, Some((ProgressTokenKey::Number(1), tx)))
            .expect("decode response");
        assert_eq!(result["content"][0]["text"], json!("ok"));
        assert!(
            rx.try_recv().is_err(),
            "malformed notifications must be ignored"
        );
    }

    #[tokio::test]
    async fn http_transport_close_is_idempotent() {
        let cfg = mcp::transport::McpServerConnectionConfig::http(
            "http-close",
            "http://127.0.0.1:9".to_string(),
        );
        let transport =
            ProgressAwareHttpTransport::connect(&cfg, None, Arc::new(Vec::new()), false).unwrap();

        transport.close().await.unwrap();
        transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn http_non_initialize_post_without_protocol_returns_session_expired() {
        let cfg = mcp::transport::McpServerConnectionConfig::http(
            "http-no-protocol",
            "http://127.0.0.1:9".to_string(),
        );
        let transport =
            ProgressAwareHttpTransport::connect(&cfg, None, Arc::new(Vec::new()), false).unwrap();

        let err = transport
            .post_message(
                JsonRpcMessage::Request(JsonRpcRequest::new(
                    JsonRpcId::Number(1),
                    "tools/call".to_string(),
                    Some(json!({})),
                )),
                true,
            )
            .await
            .expect_err("non-initialize POST without negotiated protocol must fail locally");

        assert!(
            matches!(&err, McpTransportError::ProtocolError(message) if message == MCP_SESSION_EXPIRED),
            "expected local session-expired guard, got: {err}"
        );
    }

    #[tokio::test]
    async fn per_request_sse_server_request_is_not_answered_on_new_session() {
        let cfg = mcp::transport::McpServerConnectionConfig::http(
            "http-stale-per-request-sse",
            "http://127.0.0.1:9".to_string(),
        );
        let transport =
            ProgressAwareHttpTransport::connect(&cfg, None, Arc::new(Vec::new()), false).unwrap();
        {
            let mut session = transport.session.write().await;
            session.session_id = Some("session-new".to_string());
            session.protocol_version = Some(MCP_PROTOCOL_VERSION.to_string());
            session.generation = 2;
        }

        let stale_session = HttpSessionSnapshot {
            session_id: Some("session-old".to_string()),
            protocol_version: Some(MCP_PROTOCOL_VERSION.to_string()),
            generation: 1,
        };
        let event = vec![format!(
            "data: {}",
            json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "roots/list"
            })
        )];

        let err = transport
            .process_sse_event(&event, 1, None, None, &stale_session)
            .await
            .expect_err("stale per-request SSE server request must not use the new session");

        assert!(
            matches!(&err, McpTransportError::ProtocolError(message) if message == MCP_SESSION_EXPIRED_AFTER_ACCEPT),
            "expected stale accepted-request session error before any response POST, got: {err}"
        );
    }

    #[tokio::test]
    async fn stale_per_request_sse_sampling_request_does_not_invoke_handler() {
        struct CountingSamplingHandler {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl SamplingHandler for CountingSamplingHandler {
            async fn handle_create_message(
                &self,
                _params: CreateMessageParams,
            ) -> Result<CreateMessageResult, McpTransportError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                use mcp::{Role, SamplingContent};
                Ok(CreateMessageResult {
                    role: Role::Assistant,
                    content: vec![SamplingContent::Text {
                        text: "should not run".to_string(),
                        annotations: None,
                        meta: None,
                    }],
                    model: "mock-model".to_string(),
                    stop_reason: None,
                    meta: None,
                })
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(CountingSamplingHandler {
            calls: Arc::clone(&calls),
        }) as Arc<dyn SamplingHandler>;
        let cfg = mcp::transport::McpServerConnectionConfig::http(
            "http-stale-sampling",
            "http://127.0.0.1:9".to_string(),
        );
        let transport =
            ProgressAwareHttpTransport::connect(&cfg, Some(handler), Arc::new(Vec::new()), true)
                .unwrap();
        {
            let mut session = transport.session.write().await;
            session.session_id = Some("session-new".to_string());
            session.protocol_version = Some(MCP_PROTOCOL_VERSION.to_string());
            session.generation = 2;
        }

        let stale_session = HttpSessionSnapshot {
            session_id: Some("session-old".to_string()),
            protocol_version: Some(MCP_PROTOCOL_VERSION.to_string()),
            generation: 1,
        };
        let event = vec![format!(
            "data: {}",
            json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "sampling/createMessage",
                "params": {
                    "messages": [],
                    "maxTokens": 1
                }
            })
        )];

        let err = transport
            .process_sse_event(&event, 1, None, None, &stale_session)
            .await
            .expect_err("stale sampling request must be rejected before handler execution");

        assert!(
            matches!(&err, McpTransportError::ProtocolError(message) if message == MCP_SESSION_EXPIRED_AFTER_ACCEPT),
            "expected stale accepted-request session error before handler execution, got: {err}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "stale per-request SSE sampling must not invoke local sampling handler"
        );
    }

    #[tokio::test]
    async fn http_send_initialized_request_reinitializes_if_session_clears_after_gate() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}", addr);
        let observed = Arc::new(tokio::sync::Mutex::new(Vec::<UnitHttpRequest>::new()));
        let observed_for_server = Arc::clone(&observed);

        let server_task = tokio::spawn(async move {
            let mut initialize_count = 0_usize;
            let mut initialized_count = 0_usize;
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept test request");
                let request = read_unit_http_request(&mut stream)
                    .await
                    .expect("parse test request");
                let wire_method = request.method.clone();
                let rpc_method = request.body["method"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                observed_for_server.lock().await.push(request.clone());

                if wire_method == "GET" {
                    write_unit_http_response(
                        &mut stream,
                        405,
                        "text/plain",
                        "listener disabled".to_string(),
                        &[],
                    )
                    .await;
                    continue;
                }

                match rpc_method.as_str() {
                    "initialize" => {
                        initialize_count += 1;
                        let body = json!({
                            "jsonrpc": "2.0",
                            "id": request.body["id"].clone(),
                            "result": {
                                "protocolVersion": MCP_PROTOCOL_VERSION,
                                "capabilities": {},
                                "serverInfo": {
                                    "name": "test-server",
                                    "version": "1.0.0"
                                }
                            }
                        })
                        .to_string();
                        write_unit_http_response(
                            &mut stream,
                            200,
                            "application/json",
                            body,
                            &[("MCP-Session-Id", format!("session-{initialize_count}"))],
                        )
                        .await;
                    }
                    "notifications/initialized" => {
                        initialized_count += 1;
                        write_unit_http_response(
                            &mut stream,
                            202,
                            "text/plain",
                            String::new(),
                            &[],
                        )
                        .await;
                    }
                    "tools/call" => {
                        let body = json!({
                            "jsonrpc": "2.0",
                            "id": request.body["id"].clone(),
                            "result": {
                                "content": [{"type": "text", "text": "ok"}]
                            }
                        })
                        .to_string();
                        write_unit_http_response(&mut stream, 200, "application/json", body, &[])
                            .await;
                        break;
                    }
                    other => panic!("unexpected method: {other}"),
                }

                assert!(
                    initialize_count <= 2 && initialized_count <= 2,
                    "test should not need extra handshakes"
                );
            }
        });

        let cfg = mcp::transport::McpServerConnectionConfig::http("http-race-retry", url);
        let transport =
            ProgressAwareHttpTransport::connect(&cfg, None, Arc::new(Vec::new()), false).unwrap();
        let capabilities = transport.initialize().await.expect("initial initialize");
        *transport.capabilities.lock().await = Some(capabilities);

        let session = Arc::clone(&transport.session);
        let capabilities = Arc::clone(&transport.capabilities);
        let mut session_guard = session.write().await;
        let request_fut =
            transport.send_initialized_request("tools/call", Some(json!({"name": "echo"})), None);
        tokio::pin!(request_fut);

        assert!(
            futures::poll!(&mut request_fut).is_pending(),
            "request should be parked on the held session lock after passing the capability gate"
        );

        *capabilities.lock().await = None;
        *session_guard = HttpSessionState::default();
        drop(session_guard);

        let result = request_fut
            .await
            .expect("session-expired guard should trigger reinitialize and retry");
        assert_eq!(result["content"][0]["text"], json!("ok"));
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server task should finish")
            .expect("server task should not panic");

        let requests = observed.lock().await.clone();
        let initialize_requests: Vec<_> = requests
            .iter()
            .filter(|request| request.body["method"] == "initialize")
            .collect();
        let tool_calls: Vec<_> = requests
            .iter()
            .filter(|request| request.body["method"] == "tools/call")
            .collect();

        assert_eq!(
            initialize_requests.len(),
            2,
            "request retry should create a second session"
        );
        assert_eq!(
            tool_calls.len(),
            1,
            "unversioned pre-retry call must not hit the server"
        );
        assert_eq!(
            tool_calls[0].headers.get("mcp-session-id"),
            Some(&"session-2".to_string())
        );
        assert_eq!(
            tool_calls[0].headers.get("mcp-protocol-version"),
            Some(&MCP_PROTOCOL_VERSION.to_string())
        );
    }

    #[tokio::test]
    async fn http_initialize_failure_clears_partial_session_before_retry() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}", addr);
        let observed = Arc::new(tokio::sync::Mutex::new(Vec::<UnitHttpRequest>::new()));
        let observed_for_server = Arc::clone(&observed);

        let server_task = tokio::spawn(async move {
            let mut initialize_count = 0_usize;
            let mut initialized_count = 0_usize;
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().await.expect("accept test request");
                let request = read_unit_http_request(&mut stream)
                    .await
                    .expect("parse test request");
                observed_for_server.lock().await.push(request.clone());

                if request.method == "DELETE" {
                    write_unit_http_response(&mut stream, 200, "text/plain", String::new(), &[])
                        .await;
                    continue;
                }

                let method = request.body["method"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();

                match method.as_str() {
                    "initialize" => {
                        initialize_count += 1;
                        let body = json!({
                            "jsonrpc": "2.0",
                            "id": request.body["id"].clone(),
                            "result": {
                                "protocolVersion": MCP_PROTOCOL_VERSION,
                                "capabilities": {},
                                "serverInfo": {
                                    "name": "test-server",
                                    "version": "1.0.0"
                                }
                            }
                        })
                        .to_string();
                        write_unit_http_response(
                            &mut stream,
                            200,
                            "application/json",
                            body,
                            &[("MCP-Session-Id", format!("session-{initialize_count}"))],
                        )
                        .await;
                    }
                    "notifications/initialized" => {
                        initialized_count += 1;
                        if initialized_count == 1 {
                            write_unit_http_response(
                                &mut stream,
                                400,
                                "text/plain",
                                "initialized rejected".to_string(),
                                &[],
                            )
                            .await;
                        } else {
                            write_unit_http_response(
                                &mut stream,
                                202,
                                "text/plain",
                                String::new(),
                                &[],
                            )
                            .await;
                        }
                    }
                    other => panic!("unexpected method: {other}"),
                }
            }
        });

        let cfg = mcp::transport::McpServerConnectionConfig::http("http-init-cleanup", url);
        let transport =
            ProgressAwareHttpTransport::connect(&cfg, None, Arc::new(Vec::new()), false).unwrap();

        let err = transport
            .initialize()
            .await
            .expect_err("first initialized notification is rejected");
        assert!(
            format!("{err}").contains("400"),
            "expected initialized rejection to surface, got: {err}"
        );
        let state_after_failure = transport.session.read().await.clone();
        assert!(state_after_failure.session_id.is_none());
        assert!(state_after_failure.protocol_version.is_none());

        transport
            .initialize()
            .await
            .expect("retry initialize should start from a clean session");
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server task should finish")
            .expect("server task should not panic");

        let requests = observed.lock().await.clone();
        let initialize_requests: Vec<_> = requests
            .iter()
            .filter(|request| request.body["method"] == "initialize")
            .collect();
        let initialized_requests: Vec<_> = requests
            .iter()
            .filter(|request| request.body["method"] == "notifications/initialized")
            .collect();
        let delete_requests: Vec<_> = requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .collect();

        assert_eq!(initialize_requests.len(), 2);
        assert_eq!(initialized_requests.len(), 2);
        assert_eq!(delete_requests.len(), 1);
        assert_eq!(
            delete_requests[0].headers.get("mcp-session-id"),
            Some(&"session-1".to_string()),
            "failed provisional session must be terminated"
        );
        assert_eq!(
            delete_requests[0].headers.get("mcp-protocol-version"),
            Some(&MCP_PROTOCOL_VERSION.to_string()),
            "DELETE after initialized failure must carry the negotiated protocol version"
        );
        assert_eq!(
            initialized_requests[0].headers.get("mcp-session-id"),
            Some(&"session-1".to_string()),
            "first initialized notification must exercise the partial session path"
        );
        assert!(
            !initialize_requests[1]
                .headers
                .contains_key("mcp-session-id"),
            "retry initialize must not carry the failed partial session"
        );
        assert!(
            !initialize_requests[1]
                .headers
                .contains_key("mcp-protocol-version"),
            "retry initialize must not carry the failed partial protocol"
        );
    }

    /// Spec (2025-11-25, §Streamable HTTP / Session Management):
    /// "Clients that no longer need a particular session SHOULD send an
    ///  HTTP DELETE to the MCP endpoint with the MCP-Session-Id header,
    ///  to explicitly terminate the session."
    ///
    /// Verifies that `ProgressAwareHttpTransport::close()` actually emits
    /// the DELETE with the right method, header name, and header value.
    /// Uses an ephemeral TCP listener instead of pulling in a mock-server
    /// dev-dep — the test owns its own one-shot server lifetime.
    #[tokio::test]
    async fn http_transport_close_sends_delete_with_session_id() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}", addr);

        let recorded = Arc::new(tokio::sync::Mutex::new(None::<String>));
        let recorded_clone = Arc::clone(&recorded);
        let server_task = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                *recorded_clone.lock().await = Some(String::from_utf8_lossy(&buf[..n]).to_string());
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });

        let cfg = mcp::transport::McpServerConnectionConfig::http("http-close-delete", url);
        let transport =
            ProgressAwareHttpTransport::connect(&cfg, None, Arc::new(Vec::new()), false).unwrap();
        // Pretend init already happened so close() has a session id to terminate.
        transport.session.write().await.session_id = Some("test-session-abc".into());

        transport.close().await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;

        let request = recorded
            .lock()
            .await
            .clone()
            .expect("server must have observed a request from close()");

        // Method line.
        let first_line = request.lines().next().unwrap_or("");
        assert!(
            first_line.starts_with("DELETE "),
            "close() must use HTTP DELETE; got first line: {first_line}"
        );

        // Session id header (HTTP header names are case-insensitive but the
        // spec literal is `MCP-Session-Id`).
        let has_session_header = request.lines().any(|line| {
            line.to_ascii_lowercase().starts_with("mcp-session-id:")
                && line.contains("test-session-abc")
        });
        assert!(
            has_session_header,
            "DELETE must carry the MCP-Session-Id header. Request was:\n{request}"
        );

        // Local state cleared after close().
        let session_after = transport.session.read().await.clone();
        assert!(session_after.session_id.is_none(), "session_id cleared");
    }

    #[tokio::test]
    async fn http_transport_close_is_terminal() {
        let cfg = mcp::transport::McpServerConnectionConfig::http(
            "http-close-terminal",
            "http://127.0.0.1:9",
        );
        let transport =
            ProgressAwareHttpTransport::connect(&cfg, None, Arc::new(Vec::new()), false).unwrap();

        transport.close().await.unwrap();
        transport.close().await.unwrap();

        let err = transport
            .server_capabilities()
            .await
            .expect_err("closed HTTP transport must not reinitialize");
        assert!(matches!(err, McpTransportError::ConnectionClosed));
    }

    // ── build_call_tool_meta tests ──

    #[test]
    fn build_meta_returns_none_when_no_progress_or_attribution() {
        let meta = build_call_tool_meta(None, &McpCallMetadata::default()).unwrap();
        assert!(
            meta.is_none(),
            "empty metadata + no progress => no _meta field"
        );
    }

    #[test]
    fn build_meta_includes_progress_token_alone() {
        let meta =
            build_call_tool_meta(Some(ProgressToken::Number(7)), &McpCallMetadata::default())
                .unwrap()
                .expect("Some(_meta) when progress is set");
        let obj = meta.as_object().unwrap();
        assert_eq!(obj.get("progressToken"), Some(&serde_json::json!(7)));
        assert!(!obj.contains_key("awaken/attribution"));
    }

    #[test]
    fn build_meta_namespaces_attribution_under_awaken_key() {
        let metadata = McpCallMetadata {
            agent_id: Some("research-assistant".into()),
            thread_id: Some("thr-abc".into()),
            run_id: Some("run-xyz".into()),
            call_id: Some("call-1".into()),
            parent_run_id: None,
            parent_call_id: None,
        };
        let meta = build_call_tool_meta(None, &metadata)
            .unwrap()
            .expect("Some(_meta) when attribution is set");
        let attribution = meta
            .get("awaken/attribution")
            .expect("attribution must be namespaced under awaken/attribution")
            .as_object()
            .unwrap();
        assert_eq!(
            attribution.get("agent_id"),
            Some(&serde_json::json!("research-assistant"))
        );
        assert_eq!(
            attribution.get("thread_id"),
            Some(&serde_json::json!("thr-abc"))
        );
        assert_eq!(
            attribution.get("run_id"),
            Some(&serde_json::json!("run-xyz"))
        );
        assert_eq!(
            attribution.get("call_id"),
            Some(&serde_json::json!("call-1"))
        );
        // Absent fields don't pollute the bag.
        assert!(!attribution.contains_key("parent_run_id"));
        assert!(!attribution.contains_key("parent_call_id"));
    }

    #[test]
    fn build_meta_combines_progress_token_and_attribution() {
        let metadata = McpCallMetadata {
            agent_id: Some("a1".into()),
            ..Default::default()
        };
        let meta = build_call_tool_meta(Some(ProgressToken::String("tok-1".into())), &metadata)
            .unwrap()
            .expect("Some(_meta)");
        let obj = meta.as_object().unwrap();
        // Both progress and attribution coexist in the same _meta map.
        assert!(obj.contains_key("progressToken"));
        let attribution = obj.get("awaken/attribution").unwrap().as_object().unwrap();
        assert_eq!(attribution.get("agent_id"), Some(&serde_json::json!("a1")));
    }

    #[test]
    fn build_meta_omits_empty_attribution_bag() {
        // Every attribution field set to None — the bag is empty so no
        // `awaken/attribution` key is added even though metadata was
        // technically "supplied" (Default::default()).
        let meta =
            build_call_tool_meta(Some(ProgressToken::Number(1)), &McpCallMetadata::default())
                .unwrap()
                .expect("Some(_meta)");
        let obj = meta.as_object().unwrap();
        assert!(obj.contains_key("progressToken"));
        assert!(!obj.contains_key("awaken/attribution"));
    }

    /// Spec (2025-11-25 §Cancellation): on a client-initiated cancel the
    /// client SHOULD send `notifications/cancelled` with the in-flight
    /// requestId. This test drives stdio `call_tool` against a reflective
    /// shell-script "MCP server" that:
    ///   1. logs every stdin line to a scratch file,
    ///   2. answers `initialize` so connect() succeeds,
    ///   3. holds `tools/call` indefinitely (never responds),
    /// then triggers `CancellationToken::cancel()` and asserts the
    /// transport wrote a `notifications/cancelled` line with a matching
    /// requestId and returned the cancellation sentinel error.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_call_tool_cancellation_emits_notification() {
        use serde_json::Value;

        let scratch = std::env::temp_dir().join(format!(
            "awaken-mcp-cancel-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&scratch);
        let scratch_str = scratch.to_string_lossy().to_string();

        // Mock server's `initialize` response echoes back the version
        // awaken sent (current `MCP_PROTOCOL_VERSION`) — anything else
        // would now be rejected by the negotiation check (see
        // `negotiate_protocol_version`), which is correct production
        // behaviour but would prevent this cancellation test from
        // reaching the assert.
        let script = format!(
            r#"
while IFS= read -r LINE; do
    printf '%s\n' "$LINE" >> "{scratch}"
    case "$LINE" in
        *'"method":"initialize"'*)
            ID=$(printf '%s' "$LINE" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"{version}","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"mock","version":"0"}}}}}}\n' "$ID"
            ;;
    esac
done
"#,
            scratch = scratch_str,
            version = MCP_PROTOCOL_VERSION,
        );

        let mut cfg = mcp::transport::McpServerConnectionConfig::stdio(
            "stdio-cancel",
            "/bin/sh",
            vec!["-c".to_string(), script],
        );
        cfg.timeout_secs = 30;

        let transport = Arc::new(
            ProgressAwareStdioTransport::connect(&cfg, None, Arc::new(Vec::new()), false)
                .await
                .expect("stdio transport connects"),
        );

        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();

        let transport_for_call = Arc::clone(&transport);
        let call_handle = tokio::spawn(async move {
            transport_for_call
                .call_tool(
                    "test-tool",
                    json!({}),
                    None,
                    McpCallContext {
                        cancellation: Some(cancel),
                        ..McpCallContext::default()
                    },
                )
                .await
        });

        // Give the tools/call line time to land on subprocess stdin
        // before triggering cancellation.
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel_trigger.cancel();

        let outcome = tokio::time::timeout(Duration::from_secs(5), call_handle)
            .await
            .expect("call did not return after cancellation")
            .expect("call task joined");

        match outcome {
            Err(McpTransportError::TransportError(msg)) if msg == CANCELLED_BY_CLIENT => {}
            other => panic!("expected CANCELLED_BY_CLIENT error, got: {other:?}"),
        }

        // Wait for the subprocess to flush the notification line.
        let started = Instant::now();
        let contents = loop {
            let current = fs::read_to_string(&scratch).unwrap_or_default();
            if current.contains("notifications/cancelled") {
                break current;
            }
            if started.elapsed() > Duration::from_secs(3) {
                panic!("did not observe notifications/cancelled. Got:\n{current}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };

        let cancel_line = contents
            .lines()
            .find(|l| l.contains("notifications/cancelled"))
            .expect("found cancellation line");
        let parsed: Value = serde_json::from_str(cancel_line).expect("notification is valid JSON");
        assert_eq!(parsed["method"], "notifications/cancelled");
        // tools/call is the 2nd JSON-RPC request id (initialize was #1).
        // The id allocator starts at 1 and increments; we tolerate any
        // positive id since timing-dependent setup may shift it.
        let request_id = parsed["params"]["requestId"]
            .as_i64()
            .expect("requestId must be a JSON-RPC integer id");
        assert!(
            request_id >= 1,
            "requestId must reference an in-flight call"
        );
        assert_eq!(parsed["params"]["reason"], "client run cancelled");

        let _ = std::fs::remove_file(&scratch);
    }

    /// R8 #6 regression: when the cancellation token is already
    /// cancelled at the moment `call_tool` is entered, the early
    /// pre-check returns the sentinel error WITHOUT ever attempting
    /// to send `notifications/cancelled`. A notification here would
    /// reference a requestId the server never saw (no `tools/call`
    /// was issued), confusing oncall and possibly tripping strict
    /// servers that validate requestId state.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_call_tool_skips_cancel_notification_when_pre_cancelled() {
        use serde_json::Value;

        let scratch = std::env::temp_dir().join(format!(
            "awaken-mcp-pre-cancel-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&scratch);
        let scratch_str = scratch.to_string_lossy().to_string();

        let script = format!(
            r#"
while IFS= read -r LINE; do
    printf '%s\n' "$LINE" >> "{scratch}"
    case "$LINE" in
        *'"method":"initialize"'*)
            ID=$(printf '%s' "$LINE" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"{version}","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"mock","version":"0"}}}}}}\n' "$ID"
            ;;
    esac
done
"#,
            scratch = scratch_str,
            version = MCP_PROTOCOL_VERSION,
        );

        let mut cfg = mcp::transport::McpServerConnectionConfig::stdio(
            "stdio-pre-cancel",
            "/bin/sh",
            vec!["-c".to_string(), script],
        );
        cfg.timeout_secs = 30;

        let transport =
            ProgressAwareStdioTransport::connect(&cfg, None, Arc::new(Vec::new()), false)
                .await
                .expect("stdio transport connects");

        // Pre-cancel BEFORE the call starts. The pre-check at the top
        // of call_tool must catch this and exit before allocating an
        // id, registering the sampling guard, or writing anything.
        let cancel = CancellationToken::new();
        cancel.cancel();

        let outcome = transport
            .call_tool(
                "test-tool",
                json!({}),
                None,
                McpCallContext {
                    cancellation: Some(cancel),
                    ..McpCallContext::default()
                },
            )
            .await;

        match outcome {
            Err(McpTransportError::TransportError(msg)) if msg == CANCELLED_BY_CLIENT => {}
            other => panic!("expected CANCELLED_BY_CLIENT, got: {other:?}"),
        }

        // Give the writer task a beat to flush anything pending so the
        // assertion isn't racy against in-flight writes. The intent is
        // that NOTHING tools/call-related, and NO notifications/cancelled,
        // was ever sent.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let contents = std::fs::read_to_string(&scratch).unwrap_or_default();
        let saw_cancel_notification = contents
            .lines()
            .any(|l| l.contains("notifications/cancelled"));
        let saw_tools_call = contents.lines().any(|l| l.contains("\"tools/call\""));
        assert!(
            !saw_cancel_notification,
            "pre-cancel must NOT emit notifications/cancelled. Scratch contained:\n{contents}"
        );
        assert!(
            !saw_tools_call,
            "pre-cancel must NOT send tools/call. Scratch contained:\n{contents}"
        );
        // The initialize handshake DID happen (we needed it to construct
        // the transport), so we expect to see at least one initialize.
        let saw_initialize = contents
            .lines()
            .any(|l| l.contains("\"method\":\"initialize\""));
        assert!(
            saw_initialize,
            "initialize handshake must still appear in scratch"
        );

        // Pacifier: avoid an unused-import warning when this test
        // is the only consumer of `Value` in this scope.
        let _: Option<Value> = None;
        let _ = std::fs::remove_file(&scratch);
    }

    /// End-to-end smoke test for R7 #2: the reflective stdio server
    /// emits `notifications/tools/list_changed` after `initialize`; the
    /// transport must forward the parsed `ListChangedKind::Tools` event
    /// to the host via `take_list_changed_receiver`.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_forwards_tools_list_changed_notification() {
        let script = format!(
            r#"
while IFS= read -r LINE; do
    case "$LINE" in
        *'"method":"initialize"'*)
            ID=$(printf '%s' "$LINE" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"{version}","capabilities":{{"tools":{{"listChanged":true}}}},"serverInfo":{{"name":"mock","version":"0"}}}}}}\n' "$ID"
            # Push a list_changed notification right after the
            # initialize response. The transport's reader must classify
            # it as a list_changed (NOT a progress notification) and
            # forward it to the receiver.
            printf '{{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}}\n'
            ;;
    esac
done
"#,
            version = MCP_PROTOCOL_VERSION,
        );

        let mut cfg = mcp::transport::McpServerConnectionConfig::stdio(
            "stdio-list-changed",
            "/bin/sh",
            vec!["-c".to_string(), script],
        );
        cfg.timeout_secs = 30;

        let transport =
            ProgressAwareStdioTransport::connect(&cfg, None, Arc::new(Vec::new()), false)
                .await
                .expect("stdio transport connects");

        let mut rx = transport
            .take_list_changed_receiver()
            .await
            .expect("transport exposes list_changed receiver");

        // The server fires the notification eagerly after initialize, so
        // it should arrive within a few hundred ms. Tolerate scheduling
        // jitter on slow CI by waiting up to a couple of seconds.
        let kind = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("did not observe list_changed within timeout")
            .expect("channel closed before notification arrived");
        assert_eq!(kind, ListChangedKind::Tools);

        // Receiver is one-shot — a second take returns None even when
        // the first call succeeded (and crucially does not panic).
        assert!(transport.take_list_changed_receiver().await.is_none());
    }

    /// Factory-only sampling is not a global capability: without a fixed
    /// fallback handler, background server-initiated sampling has no safe
    /// agent attribution. The manager must therefore leave `sampling`
    /// unadvertised unless a transport-level handler exists.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_does_not_advertise_sampling_without_fixed_handler() {
        let scratch = std::env::temp_dir().join(format!(
            "awaken-mcp-sampling-cap-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&scratch);
        let scratch_str = scratch.to_string_lossy().to_string();

        let script = format!(
            r#"
while IFS= read -r LINE; do
    printf '%s\n' "$LINE" >> "{scratch}"
    case "$LINE" in
        *'"method":"initialize"'*)
            ID=$(printf '%s' "$LINE" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"{version}","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"mock","version":"0"}}}}}}\n' "$ID"
            ;;
    esac
done
"#,
            scratch = scratch_str,
            version = MCP_PROTOCOL_VERSION,
        );

        let mut cfg = mcp::transport::McpServerConnectionConfig::stdio(
            "stdio-sampling-cap",
            "/bin/sh",
            vec!["-c".to_string(), script],
        );
        cfg.timeout_secs = 30;

        // sampling_handler = None (no fixed fallback), advertise_sampling = false.
        let _ = ProgressAwareStdioTransport::connect(&cfg, None, Arc::new(Vec::new()), false)
            .await
            .expect("stdio transport connects");

        // Give the writer task a beat to land the initialize line on disk.
        let started = Instant::now();
        let init_line = loop {
            let contents = std::fs::read_to_string(&scratch).unwrap_or_default();
            if let Some(line) = contents
                .lines()
                .find(|l| l.contains("\"method\":\"initialize\""))
            {
                break line.to_string();
            }
            if started.elapsed() > Duration::from_secs(3) {
                panic!(
                    "did not observe initialize request. scratch contents:\n{}",
                    contents
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };

        let parsed: serde_json::Value =
            serde_json::from_str(&init_line).expect("initialize request must be valid JSON");
        let caps = parsed["params"]["capabilities"]
            .as_object()
            .expect("capabilities object present");
        assert!(
            !caps.contains_key("sampling"),
            "factory-only sampling must not advertise global `capabilities.sampling`; got: {init_line}"
        );

        let _ = std::fs::remove_file(&scratch);
    }

    // ── Per-call sampling routing tests (R1 #P1b) ──

    /// Helper: a sampling handler that records which "agent" handled
    /// the call by storing a tag in a shared slot. Lets tests verify
    /// per-call routing picked the right one.
    struct TaggedSamplingHandler {
        tag: String,
        last_caller: Arc<tokio::sync::Mutex<Option<String>>>,
    }

    #[async_trait]
    impl SamplingHandler for TaggedSamplingHandler {
        async fn handle_create_message(
            &self,
            _params: CreateMessageParams,
        ) -> Result<mcp::CreateMessageResult, McpTransportError> {
            *self.last_caller.lock().await = Some(self.tag.clone());
            use mcp::{Role, SamplingContent};
            Ok(mcp::CreateMessageResult {
                role: Role::Assistant,
                content: vec![SamplingContent::Text {
                    text: self.tag.clone(),
                    annotations: None,
                    meta: None,
                }],
                model: "stub".into(),
                stop_reason: Some("endTurn".into()),
                meta: None,
            })
        }
    }

    #[tokio::test]
    async fn stdio_sampling_uses_only_fixed_fallback() {
        let last_caller = Arc::new(tokio::sync::Mutex::new(None::<String>));
        let per_call: PerCallSamplingHandlers = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let agent_handler: Arc<dyn SamplingHandler> = Arc::new(TaggedSamplingHandler {
            tag: "agent-A".into(),
            last_caller: Arc::clone(&last_caller),
        });
        let fallback: Arc<dyn SamplingHandler> = Arc::new(TaggedSamplingHandler {
            tag: "fallback".into(),
            last_caller: Arc::clone(&last_caller),
        });
        per_call
            .lock()
            .await
            .insert(42, PerCallSamplingEntry::Bound(agent_handler));

        let chosen = select_stdio_sampling_handler(Some(&fallback));
        let chosen = chosen.expect("a handler was selected");

        let _ = chosen
            .handle_create_message(make_minimal_sampling_params())
            .await
            .expect("handler succeeded");
        assert_eq!(
            *last_caller.lock().await,
            Some("fallback".to_string()),
            "stdio has no request correlation; it must not route sampling to the in-flight agent"
        );
    }

    #[test]
    fn stdio_sampling_rejects_factory_only_without_fixed_fallback() {
        assert!(select_stdio_sampling_handler(None).is_none());
    }

    #[tokio::test]
    async fn per_call_sampling_guard_inserts_and_removes_bound() {
        let per_call: PerCallSamplingHandlers = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let handler: Arc<dyn SamplingHandler> = Arc::new(TaggedSamplingHandler {
            tag: "x".into(),
            last_caller: Arc::new(tokio::sync::Mutex::new(None)),
        });
        {
            let _guard = PerCallSamplingGuard::register(
                Arc::clone(&per_call),
                99,
                McpCallSampling::Bound(handler),
            )
            .await;
            assert!(per_call.lock().await.contains_key(&99));
        }
        // After guard drops the entry should be gone. Tolerate a brief
        // yield for the spawned-removal path; in the happy path try_lock
        // succeeds and removal is synchronous.
        tokio::task::yield_now().await;
        assert!(!per_call.lock().await.contains_key(&99));
    }

    #[tokio::test]
    async fn per_call_sampling_guard_inserts_and_removes_denied() {
        let per_call: PerCallSamplingHandlers = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        {
            let _guard =
                PerCallSamplingGuard::register(Arc::clone(&per_call), 7, McpCallSampling::Denied)
                    .await;
            assert!(matches!(
                per_call.lock().await.get(&7),
                Some(PerCallSamplingEntry::Denied)
            ));
        }
        tokio::task::yield_now().await;
        assert!(!per_call.lock().await.contains_key(&7));
    }

    #[tokio::test]
    async fn per_call_sampling_guard_inherit_registers_nothing() {
        // Inherit semantics: caller didn't engage the factory, so no
        // per-call entry is registered. The transport's reader path
        // sees an empty map and uses its fallback handler.
        let per_call: PerCallSamplingHandlers = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        {
            let _guard =
                PerCallSamplingGuard::register(Arc::clone(&per_call), 3, McpCallSampling::Inherit)
                    .await;
            assert!(
                per_call.lock().await.is_empty(),
                "Inherit registers nothing"
            );
        }
        // Drop is a no-op for Inherit — map remains empty.
        tokio::task::yield_now().await;
        assert!(per_call.lock().await.is_empty());
    }

    fn make_minimal_sampling_params() -> CreateMessageParams {
        use mcp::SamplingMessage;
        CreateMessageParams {
            messages: vec![SamplingMessage {
                role: mcp::Role::User,
                content: vec![mcp::SamplingContent::Text {
                    text: "hi".into(),
                    annotations: None,
                    meta: None,
                }],
                meta: None,
            }],
            model_preferences: None,
            system_prompt: None,
            include_context: None,
            temperature: None,
            max_tokens: 16,
            stop_sequences: None,
            metadata: None,
            tools: None,
            tool_choice: None,
            task: None,
            meta: None,
        }
    }

    #[test]
    fn build_meta_includes_parent_when_present() {
        let metadata = McpCallMetadata {
            agent_id: Some("delegate".into()),
            parent_run_id: Some("parent-run".into()),
            parent_call_id: Some("parent-call".into()),
            ..Default::default()
        };
        let attribution = build_call_tool_meta(None, &metadata)
            .unwrap()
            .unwrap()
            .get("awaken/attribution")
            .cloned()
            .unwrap();
        let obj = attribution.as_object().unwrap();
        assert_eq!(
            obj.get("parent_run_id"),
            Some(&serde_json::json!("parent-run"))
        );
        assert_eq!(
            obj.get("parent_call_id"),
            Some(&serde_json::json!("parent-call"))
        );
    }

    /// close() with no session id MUST NOT emit any HTTP request — there's
    /// nothing to terminate. Pairs with the test above so a refactor that
    /// accidentally always-DELETEs trips the empty-session case.
    #[tokio::test]
    async fn http_transport_close_without_session_emits_no_request() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}", addr);

        let observed = Arc::new(tokio::sync::Mutex::new(false));
        let observed_clone = Arc::clone(&observed);
        let server_task = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 16];
                if stream.read(&mut buf).await.unwrap_or(0) > 0 {
                    *observed_clone.lock().await = true;
                }
            }
        });

        let cfg = mcp::transport::McpServerConnectionConfig::http("http-close-no-session", url);
        let transport =
            ProgressAwareHttpTransport::connect(&cfg, None, Arc::new(Vec::new()), false).unwrap();
        // No session id set.
        transport.close().await.unwrap();

        // Give the listener a brief moment to observe a spurious request.
        let _ = tokio::time::timeout(Duration::from_millis(150), server_task).await;
        assert!(
            !*observed.lock().await,
            "close() with no session_id must not contact the server"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_connect_init_failure_cleans_up_child_process() {
        let pid_file = format!(
            "/tmp/awaken-ext-mcp-stdio-cleanup-{}.pid",
            std::process::id()
        );
        let _ = fs::remove_file(&pid_file);

        let mut cfg = mcp::transport::McpServerConnectionConfig::stdio(
            "stdio-cleanup",
            "/bin/sh",
            vec![
                "-c".to_string(),
                format!("echo $$ > \"{pid_file}\"; trap 'exit 0' INT TERM; sleep 30"),
            ],
        );
        cfg.timeout_secs = 1;

        let err =
            match ProgressAwareStdioTransport::connect(&cfg, None, Arc::new(Vec::new()), false)
                .await
            {
                Ok(_) => panic!("expected stdio initialization failure"),
                Err(err) => err,
            };
        assert!(matches!(err, McpTransportError::Timeout(_)));

        let started = Instant::now();
        let pid = loop {
            if let Ok(contents) = fs::read_to_string(&pid_file)
                && let Ok(pid) = contents.trim().parse::<i32>()
            {
                break pid;
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match nix::sys::signal::kill(Pid::from_raw(pid), None) {
                Ok(()) => {
                    assert!(
                        Instant::now() < deadline,
                        "child process was not cleaned up"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(nix::errno::Errno::ESRCH) => break,
                Err(err) => panic!("unexpected process status error: {err}"),
            }
        }

        let _ = fs::remove_file(&pid_file);
    }

    #[test]
    fn decode_http_batch_emits_progress_before_and_after_response_in_order() {
        let (tx, mut rx) = mpsc::channel(MCP_PROGRESS_CHANNEL_CAPACITY);
        let body = json!([
            {
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"progressToken": 7, "progress": 1.0, "total": 4.0, "message": "before"}
            },
            {
                "jsonrpc": "2.0",
                "id": 3,
                "result": {"content": [{"type": "text", "text": "ok"}]}
            },
            {
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"progressToken": 7, "progress": 4.0, "total": 4.0, "message": "after"}
            }
        ]);

        let result = decode_http_response_payload(body, 3, Some((ProgressTokenKey::Number(7), tx)))
            .expect("decode response");

        let first = rx.try_recv().expect("first progress");
        let second = rx.try_recv().expect("second progress");
        assert_eq!(first.message.as_deref(), Some("before"));
        assert_eq!(second.message.as_deref(), Some("after"));
        assert_eq!(result["content"][0]["text"], json!("ok"));
    }

    #[test]
    fn plain_text_content_joins_text_items() {
        let content = vec![ToolContent::text("hello"), ToolContent::text("world")];
        assert_eq!(
            plain_text_content(&content),
            Some("hello\nworld".to_string())
        );
    }

    #[test]
    fn plain_text_content_returns_none_for_mixed() {
        let content = vec![ToolContent::Resource {
            uri: "file://x".to_string(),
            mime_type: None,
        }];
        assert!(plain_text_content(&content).is_none());
    }

    #[test]
    fn call_result_to_data_plain_text() {
        let result = CallToolResult {
            content: vec![ToolContent::text("hello")],
            structured_content: None,
            is_error: None,
        };
        assert_eq!(call_result_to_tool_data(&result), json!("hello"));
    }

    #[test]
    fn call_result_to_data_structured() {
        let result = CallToolResult {
            content: vec![ToolContent::text("ok")],
            structured_content: Some(json!({"key": "value"})),
            is_error: None,
        };
        let data = call_result_to_tool_data(&result);
        assert_eq!(data["structuredContent"]["key"], json!("value"));
    }

    #[test]
    fn tool_result_error_text_from_text_content() {
        let result = CallToolResult {
            content: vec![ToolContent::text("error message")],
            structured_content: None,
            is_error: Some(true),
        };
        assert_eq!(tool_result_error_text(&result), "error message");
    }

    #[test]
    fn tool_result_error_text_from_structured() {
        let result = CallToolResult {
            content: vec![],
            structured_content: Some(json!({"error": "structured"})),
            is_error: Some(true),
        };
        assert!(tool_result_error_text(&result).contains("structured"));
    }

    #[test]
    fn tool_result_error_text_empty() {
        let result = CallToolResult {
            content: vec![],
            structured_content: None,
            is_error: Some(true),
        };
        assert_eq!(tool_result_error_text(&result), "Unknown error");
    }

    // ── ProgressTokenKey conversion tests ──

    #[test]
    fn progress_token_key_from_string() {
        let token = ProgressToken::String("abc".to_string());
        let key = ProgressTokenKey::from(&token);
        assert_eq!(key, ProgressTokenKey::String("abc".to_string()));
    }

    #[test]
    fn progress_token_key_from_number() {
        let token = ProgressToken::Number(42);
        let key = ProgressTokenKey::from(&token);
        assert_eq!(key, ProgressTokenKey::Number(42));
    }

    #[test]
    fn progress_token_key_equality() {
        assert_eq!(
            ProgressTokenKey::String("x".to_string()),
            ProgressTokenKey::String("x".to_string())
        );
        assert_ne!(
            ProgressTokenKey::String("x".to_string()),
            ProgressTokenKey::Number(0)
        );
        assert_eq!(ProgressTokenKey::Number(1), ProgressTokenKey::Number(1));
        assert_ne!(ProgressTokenKey::Number(1), ProgressTokenKey::Number(2));
    }

    // ── initialize_params tests ──

    #[test]
    fn initialize_params_structure() {
        let params = initialize_params(json!({"sampling": {}}), json!({"key": "val"}));
        assert_eq!(params["protocolVersion"], json!(MCP_PROTOCOL_VERSION));
        assert!(params["clientInfo"]["name"].as_str().is_some());
        assert_eq!(params["capabilities"]["sampling"], json!({}));
        assert_eq!(params["config"]["key"], json!("val"));
    }

    #[test]
    fn initialize_params_empty_capabilities() {
        let params = initialize_params(json!({}), Value::Null);
        assert_eq!(params["capabilities"], json!({}));
        assert_eq!(params["config"], Value::Null);
    }

    // ── map_response_payload tests ──

    #[test]
    fn map_response_payload_success() {
        let payload = JsonRpcPayload::Success {
            result: json!({"tools": []}),
        };
        let result = map_response_payload(payload).unwrap();
        assert_eq!(result, json!({"tools": []}));
    }

    #[test]
    fn map_response_payload_error() {
        let payload = JsonRpcPayload::Error {
            error: mcp::JsonRpcError {
                code: -32600,
                message: "bad request".to_string(),
                data: None,
            },
        };
        let result = map_response_payload(payload);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, McpTransportError::ServerError(_)));
    }

    // ── parse_json_rpc_message tests ──

    #[test]
    fn parse_json_rpc_message_valid_response() {
        let val = json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}});
        let msg = parse_json_rpc_message(val).unwrap();
        assert!(matches!(msg, JsonRpcMessage::Response(_)));
    }

    #[test]
    fn parse_json_rpc_message_valid_notification() {
        let val = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}});
        let msg = parse_json_rpc_message(val).unwrap();
        assert!(matches!(msg, JsonRpcMessage::Notification(_)));
    }

    #[test]
    fn parse_json_rpc_message_invalid_returns_error() {
        let val = json!({"not_jsonrpc": true});
        let result = parse_json_rpc_message(val);
        assert!(result.is_err());
    }

    // ── decode_progress_notification tests ──

    #[test]
    fn decode_progress_notification_non_progress_method() {
        let notification = JsonRpcNotification::new("notifications/other", Some(json!({})));
        assert!(decode_progress_notification(notification).is_none());
    }

    #[test]
    fn decode_progress_notification_missing_params() {
        let notification = JsonRpcNotification::new("notifications/progress", None);
        assert!(decode_progress_notification(notification).is_none());
    }

    #[test]
    fn decode_progress_notification_valid_string_token() {
        let notification = JsonRpcNotification::new(
            "notifications/progress",
            Some(json!({
                "progressToken": "tok-1",
                "progress": 0.5,
                "total": 1.0,
                "message": "halfway"
            })),
        );
        let (key, update) = decode_progress_notification(notification).unwrap();
        assert_eq!(key, ProgressTokenKey::String("tok-1".to_string()));
        assert!((update.progress - 0.5).abs() < f64::EPSILON);
        assert_eq!(update.total, Some(1.0));
        assert_eq!(update.message.as_deref(), Some("halfway"));
    }

    #[test]
    fn decode_progress_notification_valid_number_token() {
        let notification = JsonRpcNotification::new(
            "notifications/progress",
            Some(json!({
                "progressToken": 99,
                "progress": 3.0,
            })),
        );
        let (key, update) = decode_progress_notification(notification).unwrap();
        assert_eq!(key, ProgressTokenKey::Number(99));
        assert!((update.progress - 3.0).abs() < f64::EPSILON);
        assert!(update.total.is_none());
        assert!(update.message.is_none());
    }

    // ── decode_http_response_payload single response ──

    #[test]
    fn decode_http_response_single_matching_id() {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": {"data": "ok"}
        });
        let result = decode_http_response_payload(body, 7, None).unwrap();
        assert_eq!(result["data"], json!("ok"));
    }

    #[test]
    fn decode_http_response_single_mismatched_id() {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": {"data": "ok"}
        });
        let err = decode_http_response_payload(body, 99, None).unwrap_err();
        assert!(matches!(err, McpTransportError::ProtocolError(_)));
    }

    #[test]
    fn decode_http_response_error_payload() {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32600, "message": "Invalid request"}
        });
        let err = decode_http_response_payload(body, 1, None).unwrap_err();
        assert!(matches!(err, McpTransportError::ServerError(_)));
    }

    // ── plain_text_content edge cases ──

    #[test]
    fn plain_text_content_empty() {
        let content: Vec<ToolContent> = vec![];
        assert_eq!(plain_text_content(&content), Some(String::new()));
    }

    #[test]
    fn plain_text_content_single_item() {
        let content = vec![ToolContent::text("only")];
        assert_eq!(plain_text_content(&content), Some("only".to_string()));
    }

    #[test]
    fn plain_text_content_with_annotations_returns_none() {
        let content = vec![ToolContent::Text {
            text: "has annotation".to_string(),
            annotations: Some(mcp::Annotations {
                audience: None,
                priority: Some(1.0),
                last_modified: None,
            }),
            meta: None,
        }];
        assert!(plain_text_content(&content).is_none());
    }

    #[test]
    fn plain_text_content_with_meta_returns_none() {
        let content = vec![ToolContent::Text {
            text: "has meta".to_string(),
            annotations: None,
            meta: Some(json!({"key": "val"})),
        }];
        assert!(plain_text_content(&content).is_none());
    }

    // ── call_result_to_tool_data edge cases ──

    #[test]
    fn call_result_to_data_empty_content() {
        let result = CallToolResult {
            content: vec![],
            structured_content: None,
            is_error: None,
        };
        // Empty content with no structured_content -> empty plain text
        assert_eq!(call_result_to_tool_data(&result), json!(""));
    }

    #[test]
    fn call_result_to_data_multiple_text() {
        let result = CallToolResult {
            content: vec![ToolContent::text("a"), ToolContent::text("b")],
            structured_content: None,
            is_error: None,
        };
        assert_eq!(call_result_to_tool_data(&result), json!("a\nb"));
    }

    // ── Serde roundtrip tests for prompt/resource types ──

    #[test]
    fn prompt_definition_serde_roundtrip() {
        let def = McpPromptDefinition {
            name: "greet".to_string(),
            title: Some("Greeting prompt".to_string()),
            description: Some("Says hello".to_string()),
            arguments: vec![McpPromptArgument {
                name: "name".to_string(),
                description: Some("Who to greet".to_string()),
                required: true,
            }],
        };
        let json = serde_json::to_string(&def).unwrap();
        let parsed: McpPromptDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, def);
    }

    #[test]
    fn prompt_definition_minimal_serde() {
        let def = McpPromptDefinition {
            name: "min".to_string(),
            title: None,
            description: None,
            arguments: vec![],
        };
        let json = serde_json::to_string(&def).unwrap();
        // Optional fields should be skipped
        assert!(!json.contains("title"));
        assert!(!json.contains("description"));
        let parsed: McpPromptDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, def);
    }

    #[test]
    fn resource_definition_serde_roundtrip() {
        let def = McpResourceDefinition {
            uri: "file://test.txt".to_string(),
            name: "test".to_string(),
            title: Some("Test file".to_string()),
            description: Some("A test resource".to_string()),
            mime_type: Some("text/plain".to_string()),
            size: Some(1024),
        };
        let json = serde_json::to_string(&def).unwrap();
        let parsed: McpResourceDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, def);
    }

    #[test]
    fn resource_definition_minimal_serde() {
        let def = McpResourceDefinition {
            uri: "file://x".to_string(),
            name: "x".to_string(),
            title: None,
            description: None,
            mime_type: None,
            size: None,
        };
        let json = serde_json::to_string(&def).unwrap();
        assert!(!json.contains("title"));
        assert!(!json.contains("mimeType"));
        let parsed: McpResourceDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, def);
    }

    #[test]
    fn prompt_result_serde_roundtrip() {
        let result = McpPromptResult {
            description: Some("Test prompt".to_string()),
            messages: vec![McpPromptMessage {
                role: "user".to_string(),
                content: json!([{"type": "text", "text": "Hello"}]),
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: McpPromptResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, result);
    }

    #[test]
    fn prompt_argument_required_defaults_to_false() {
        let json = r#"{"name": "arg1"}"#;
        let arg: McpPromptArgument = serde_json::from_str(json).unwrap();
        assert_eq!(arg.name, "arg1");
        assert!(!arg.required);
        assert!(arg.description.is_none());
    }

    // ── tool_result_error_text with non-text content ──

    #[test]
    fn tool_result_error_text_non_text_content_serialized() {
        let result = CallToolResult {
            content: vec![ToolContent::Resource {
                uri: "file://x".to_string(),
                mime_type: Some("text/plain".to_string()),
            }],
            structured_content: None,
            is_error: Some(true),
        };
        // No text content, no structured_content, but content is non-empty -> serialized
        let text = tool_result_error_text(&result);
        assert!(text.contains("file://x"));
    }

    // ── initialize_params additional tests ──

    #[test]
    fn initialize_params_client_info_has_name_and_version() {
        let params = initialize_params(json!({}), Value::Null);
        assert_eq!(params["clientInfo"]["name"], json!("awaken-mcp"));
        let version = params["clientInfo"]["version"].as_str().unwrap();
        assert!(!version.is_empty());
    }

    #[test]
    fn initialize_params_nested_capabilities() {
        let caps = json!({
            "sampling": {},
            "experimental": {"feature_x": true}
        });
        let params = initialize_params(caps.clone(), json!(null));
        assert_eq!(params["capabilities"], caps);
    }

    #[test]
    fn initialize_params_complex_config() {
        let config = json!({
            "key1": "val1",
            "nested": {"a": [1, 2, 3]}
        });
        let params = initialize_params(json!({}), config.clone());
        assert_eq!(params["config"], config);
    }

    // ── map_response_payload additional tests ──

    #[test]
    fn map_response_payload_success_null_result() {
        let payload = JsonRpcPayload::Success {
            result: Value::Null,
        };
        let result = map_response_payload(payload).unwrap();
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn map_response_payload_error_contains_code_and_message() {
        let payload = JsonRpcPayload::Error {
            error: mcp::JsonRpcError {
                code: -32601,
                message: "Method not found".to_string(),
                data: Some(json!({"detail": "extra info"})),
            },
        };
        let err = map_response_payload(payload).unwrap_err();
        match err {
            McpTransportError::ServerError(msg) => {
                assert!(msg.contains("Method not found"));
            }
            other => panic!("expected ServerError, got {:?}", other),
        }
    }

    #[test]
    fn map_response_payload_success_array_result() {
        let payload = JsonRpcPayload::Success {
            result: json!([1, 2, 3]),
        };
        let result = map_response_payload(payload).unwrap();
        assert_eq!(result, json!([1, 2, 3]));
    }

    // ── parse_json_rpc_message additional tests ──

    #[test]
    fn parse_json_rpc_message_request() {
        let val = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {"name": "test"}
        });
        let msg = parse_json_rpc_message(val).unwrap();
        assert!(matches!(msg, JsonRpcMessage::Request(_)));
    }

    #[test]
    fn parse_json_rpc_message_error_response() {
        let val = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "error": {"code": -32600, "message": "Invalid"}
        });
        let msg = parse_json_rpc_message(val).unwrap();
        match msg {
            JsonRpcMessage::Response(resp) => {
                assert!(matches!(resp.payload, JsonRpcPayload::Error { .. }));
            }
            other => panic!("expected Response, got {:?}", other),
        }
    }

    #[test]
    fn parse_json_rpc_message_fallback_requires_jsonrpc_field() {
        // Both primary and fallback paths require the jsonrpc field,
        // so omitting it returns an error.
        let val = json!({
            "id": 1,
            "result": {"ok": true}
        });
        assert!(parse_json_rpc_message(val).is_err());
    }

    // ── decode_http_response_payload additional tests ──

    #[test]
    fn decode_http_response_empty_batch_returns_missing_response() {
        let body = json!([]);
        let err = decode_http_response_payload(body, 1, None).unwrap_err();
        assert!(matches!(err, McpTransportError::ProtocolError(_)));
    }

    #[test]
    fn decode_http_response_batch_with_only_notifications() {
        let body = json!([
            {
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"progressToken": 1, "progress": 1.0}
            }
        ]);
        let err = decode_http_response_payload(body, 1, None).unwrap_err();
        assert!(matches!(err, McpTransportError::ProtocolError(_)));
    }

    #[test]
    fn decode_http_response_batch_request_messages_ignored() {
        let body = json!([
            {
                "jsonrpc": "2.0",
                "id": 100,
                "method": "sampling/createMessage",
                "params": {"messages": [], "maxTokens": 10}
            },
            {
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"data": "found"}
            }
        ]);
        let result = decode_http_response_payload(body, 1, None).unwrap();
        assert_eq!(result["data"], json!("found"));
    }

    #[test]
    fn decode_http_response_progress_not_emitted_without_registration() {
        let body = json!([
            {
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"progressToken": 5, "progress": 1.0, "message": "step"}
            },
            {
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"ok": true}
            }
        ]);
        // No progress registration: progress notification is silently ignored
        let result = decode_http_response_payload(body, 2, None).unwrap();
        assert_eq!(result["ok"], json!(true));
    }

    #[test]
    fn decode_http_response_progress_token_mismatch_not_emitted() {
        let (tx, mut rx) = mpsc::channel(MCP_PROGRESS_CHANNEL_CAPACITY);
        let body = json!([
            {
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"progressToken": 99, "progress": 1.0}
            },
            {
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"ok": true}
            }
        ]);
        let result =
            decode_http_response_payload(body, 1, Some((ProgressTokenKey::Number(1), tx))).unwrap();
        assert_eq!(result["ok"], json!(true));
        assert!(
            rx.try_recv().is_err(),
            "mismatched token must not emit progress"
        );
    }

    #[test]
    fn decode_http_response_single_notification_no_response() {
        let body = json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {"progressToken": 1, "progress": 0.5}
        });
        let err = decode_http_response_payload(body, 1, None).unwrap_err();
        assert!(matches!(err, McpTransportError::ProtocolError(_)));
    }

    #[test]
    fn decode_http_response_invalid_item_in_batch_returns_error() {
        let body = json!([
            {"not_jsonrpc": true}
        ]);
        let result = decode_http_response_payload(body, 1, None);
        assert!(result.is_err());
    }

    #[test]
    fn decode_http_response_single_invalid_returns_error() {
        let body = json!({"random": "data"});
        let result = decode_http_response_payload(body, 1, None);
        assert!(result.is_err());
    }

    #[test]
    fn decode_http_response_progress_with_string_token() {
        let (tx, mut rx) = mpsc::channel(MCP_PROGRESS_CHANNEL_CAPACITY);
        let body = json!([
            {
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"progressToken": "tok-abc", "progress": 2.0, "total": 5.0}
            },
            {
                "jsonrpc": "2.0",
                "id": 4,
                "result": {"done": true}
            }
        ]);
        let result = decode_http_response_payload(
            body,
            4,
            Some((ProgressTokenKey::String("tok-abc".to_string()), tx)),
        )
        .unwrap();
        assert_eq!(result["done"], json!(true));
        let update = rx.try_recv().expect("should receive progress");
        assert!((update.progress - 2.0).abs() < f64::EPSILON);
        assert_eq!(update.total, Some(5.0));
    }

    // ── decode_progress_notification additional tests ──

    #[test]
    fn decode_progress_notification_malformed_params() {
        let notification = JsonRpcNotification::new(
            "notifications/progress",
            Some(json!({"progressToken": {"bad": true}, "progress": "not_a_number"})),
        );
        // Malformed params should fail serde and return None
        assert!(decode_progress_notification(notification).is_none());
    }

    // ── tool_result_error_text additional tests ──

    #[test]
    fn tool_result_error_text_multiple_text_items_joined() {
        let result = CallToolResult {
            content: vec![ToolContent::text("line1"), ToolContent::text("line2")],
            structured_content: None,
            is_error: Some(true),
        };
        assert_eq!(tool_result_error_text(&result), "line1\nline2");
    }

    #[test]
    fn tool_result_error_text_structured_takes_precedence_over_empty_text() {
        // When content has no text items but structured_content exists
        let result = CallToolResult {
            content: vec![ToolContent::Resource {
                uri: "file://r".to_string(),
                mime_type: None,
            }],
            structured_content: Some(json!({"err": "details"})),
            is_error: Some(true),
        };
        // Text filter_map yields nothing since Resource has no as_text(),
        // so text is empty -> falls through to structured
        let text = tool_result_error_text(&result);
        assert!(text.contains("details"));
    }

    // ── call_result_to_tool_data additional tests ──

    #[test]
    fn call_result_to_data_non_text_content_serialized() {
        let result = CallToolResult {
            content: vec![ToolContent::Resource {
                uri: "file://test".to_string(),
                mime_type: Some("application/json".to_string()),
            }],
            structured_content: None,
            is_error: None,
        };
        // plain_text_content returns None -> falls to serde serialization
        let data = call_result_to_tool_data(&result);
        assert!(data["content"][0]["uri"].as_str().is_some());
    }

    #[test]
    fn call_result_to_data_text_with_annotations_serialized() {
        let result = CallToolResult {
            content: vec![ToolContent::Text {
                text: "annotated".to_string(),
                annotations: Some(mcp::Annotations {
                    audience: None,
                    priority: Some(0.5),
                    last_modified: None,
                }),
                meta: None,
            }],
            structured_content: None,
            is_error: None,
        };
        // plain_text_content returns None for annotated items -> serialized as JSON
        let data = call_result_to_tool_data(&result);
        assert!(data.is_object());
        assert_eq!(data["content"][0]["text"], json!("annotated"));
    }

    // ── plain_text_content additional tests ──

    #[test]
    fn plain_text_content_with_both_annotations_and_meta_returns_none() {
        let content = vec![ToolContent::Text {
            text: "both".to_string(),
            annotations: Some(mcp::Annotations {
                audience: None,
                priority: Some(1.0),
                last_modified: None,
            }),
            meta: Some(json!({"k": "v"})),
        }];
        assert!(plain_text_content(&content).is_none());
    }

    #[test]
    fn plain_text_content_mixed_plain_and_annotated_returns_none() {
        let content = vec![
            ToolContent::text("plain"),
            ToolContent::Text {
                text: "annotated".to_string(),
                annotations: Some(mcp::Annotations {
                    audience: None,
                    priority: Some(0.1),
                    last_modified: None,
                }),
                meta: None,
            },
        ];
        assert!(plain_text_content(&content).is_none());
    }

    // ── handle_server_request additional tests ──

    #[tokio::test]
    async fn handle_sampling_request_with_no_params() {
        let handler = MockSamplingHandler {
            response_text: "unused".to_string(),
        };
        let request = JsonRpcRequest::new(
            JsonRpcId::Number(10),
            "sampling/createMessage".to_string(),
            None,
        );
        let response = handle_server_request(Some(&handler), &[], &request).await;
        match response.payload {
            mcp::JsonRpcPayload::Error { error } => {
                assert!(error.to_string().contains("Invalid sampling/createMessage"));
            }
            _ => panic!("expected error for missing params"),
        }
    }

    #[tokio::test]
    async fn handle_unknown_method_with_handler_still_returns_not_found() {
        let handler = MockSamplingHandler {
            response_text: "unused".to_string(),
        };
        let request = JsonRpcRequest::new(
            JsonRpcId::Number(20),
            "tools/call".to_string(),
            Some(json!({})),
        );
        let response = handle_server_request(Some(&handler), &[], &request).await;
        match response.payload {
            mcp::JsonRpcPayload::Error { error } => {
                assert!(error.to_string().contains("Method not supported"));
                assert!(error.to_string().contains("tools/call"));
            }
            _ => panic!("expected error response"),
        }
    }

    // ── ProgressTokenKey hash consistency ──

    #[test]
    fn progress_token_key_works_as_hashmap_key() {
        let mut map = HashMap::new();
        map.insert(ProgressTokenKey::String("a".to_string()), 1);
        map.insert(ProgressTokenKey::Number(42), 2);
        assert_eq!(
            map.get(&ProgressTokenKey::String("a".to_string())),
            Some(&1)
        );
        assert_eq!(map.get(&ProgressTokenKey::Number(42)), Some(&2));
        assert_eq!(map.get(&ProgressTokenKey::String("b".to_string())), None);
        assert_eq!(map.get(&ProgressTokenKey::Number(0)), None);
    }
}
