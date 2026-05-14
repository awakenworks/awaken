//! MCP tool transport: wraps MCP tool calls as awaken `Tool` implementations.
//!
//! Contains the `McpToolTransport` trait (raw MCP client abstraction) and
//! `McpTool` which adapts an MCP tool definition into an awaken `Tool`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use mcp::transport::{
    ClientInfo, InitializeCapabilities, InitializeResult, McpServerConnectionConfig,
    McpTransportError, SamplingCapabilities, ServerCapabilities, TransportTypeId,
};
use mcp::{
    CallToolParams, CallToolResult, CreateMessageParams, JsonRpcId, JsonRpcMessage,
    JsonRpcNotification, JsonRpcPayload, JsonRpcRequest, JsonRpcResponse, ListToolsResult,
    MCP_PROTOCOL_VERSION, McpToolDefinition, ProgressNotificationParams, ProgressToken,
    ToolContent,
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
/// Spec (2025-06-18 §JSON-RPC 2.0 + §Basic) reserves the `_meta` field on
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
#[derive(Default)]
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
        progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
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

    /// Current server-assigned session id, if any. Streamable HTTP
    /// transports return the value cached after the most recent
    /// successful `initialize`; stdio returns `None` (the protocol does
    /// not define a session id for that transport). Display-only — do
    /// not cache or persist outside the transport.
    async fn current_session_id(&self) -> Option<String> {
        None
    }
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
}

impl Drop for PerCallSamplingGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // Drop is sync; we can only try to lock. The map is taken only
        // briefly by the reader task or other guards, so try_lock should
        // succeed in the common case. If contended, schedule removal so
        // the entry doesn't outlive the call indefinitely.
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
    progress_subscribers: Arc<
        tokio::sync::Mutex<HashMap<ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>>>,
    >,
    next_id: AtomicI64,
    next_progress_token: AtomicI64,
    alive: Arc<AtomicBool>,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
    timeout: Duration,
    capabilities: Option<ServerCapabilities>,
    /// Map of in-flight tool-call JSON-RPC ids → per-call sampling
    /// decision (Bound | Denied). Populated by `call_tool` whenever the
    /// caller supplies a `McpCallSampling` other than `Inherit`; emptied
    /// via the `PerCallSamplingGuard` on every exit path. Stdio cannot
    /// correlate a server-initiated `sampling/createMessage` to a
    /// specific in-flight `tools/call` (no spec-mandated id field), so
    /// the reader uses cardinality heuristics over this map — see
    /// [`select_sampling_handler`].
    per_call_sampling: PerCallSamplingHandlers,
}

impl ProgressAwareStdioTransport {
    pub(crate) async fn connect(
        config: &McpServerConnectionConfig,
        sampling_handler: Option<Arc<dyn SamplingHandler>>,
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
            tokio::sync::Mutex<HashMap<ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>>>,
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
        let per_call_sampling_reader = Arc::clone(&per_call_sampling);
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
                            handle_progress_notification(&progress_reader, notification).await;
                        }
                        Ok(JsonRpcMessage::Request(request)) => {
                            let fallback = sampling_handler_reader.clone();
                            let per_call = Arc::clone(&per_call_sampling_reader);
                            let wtx = write_tx_reader.clone();
                            tokio::spawn(async move {
                                let chosen =
                                    select_sampling_handler(&per_call, fallback.as_ref()).await;
                                let response =
                                    handle_server_request(chosen.as_deref(), &request).await;
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
        };

        let mut capabilities = InitializeCapabilities::default();
        if sampling_handler.is_some() {
            capabilities.sampling = Some(SamplingCapabilities::default());
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
        progress_registration: Option<(ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>)>,
    ) -> Result<Value, McpTransportError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.send_request_with_id(id, method, params, progress_registration)
            .await
    }

    /// Send a JSON-RPC request using a caller-supplied id. Extracted from
    /// `send_request` so cancellable call paths can allocate the id up
    /// front and reuse it when emitting `notifications/cancelled`
    /// (which references the in-flight request by id per spec).
    async fn send_request_with_id(
        &self,
        id: i64,
        method: &str,
        params: Option<Value>,
        progress_registration: Option<(ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>)>,
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
        progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
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
        let _handler_guard =
            PerCallSamplingGuard::register(Arc::clone(&self.per_call_sampling), id, sampling).await;

        let request_fut = self.send_request_with_id(
            id,
            "tools/call",
            Some(serde_json::to_value(&params)?),
            progress_sender,
        );

        let result = match cancellation {
            None => request_fut.await,
            Some(token) => {
                tokio::pin!(request_fut);
                tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        // Per spec (2025-06-18 §Cancellation): the client
                        // SHOULD send `notifications/cancelled` with the
                        // in-flight requestId so the server can stop
                        // processing and free resources. We use the
                        // flushed variant because the transport is
                        // typically dropped immediately after this
                        // returns; `kill_on_drop(true)` would race the
                        // notification write and the server might never
                        // see the cancellation.
                        let _ = self
                            .send_notification_flushed(
                                "notifications/cancelled",
                                Some(json!({
                                    "requestId": id,
                                    "reason": "client run cancelled",
                                })),
                            )
                            .await;
                        // Drop the pending entry so a late response is
                        // dropped on the reader floor rather than
                        // delivered to an orphaned channel.
                        self.forget_pending(id).await;
                        Err(McpTransportError::TransportError(
                            CANCELLED_BY_CLIENT.to_string(),
                        ))
                    }
                    result = &mut request_fut => result,
                }
            }
        };

        let result = result?;
        let call_result: CallToolResult = serde_json::from_value(result)?;

        if call_result.is_error == Some(true) {
            return Err(McpTransportError::ServerError(tool_result_error_text(
                &call_result,
            )));
        }

        Ok(call_result)
    }

    fn transport_type(&self) -> TransportTypeId {
        TransportTypeId::Stdio
    }

    async fn server_capabilities(&self) -> Result<Option<ServerCapabilities>, McpTransportError> {
        Ok(self.capabilities.clone())
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
    next_id: AtomicI64,
    next_progress_token: AtomicI64,
    capabilities: tokio::sync::Mutex<Option<ServerCapabilities>>,
    session: tokio::sync::RwLock<HttpSessionState>,
    sampling_handler: Option<Arc<dyn SamplingHandler>>,
    /// Per-call sampling handlers; same semantics as the stdio variant.
    /// Populated by `call_tool` while a request is in flight; consulted
    /// by `handle_server_request` for `sampling/createMessage`. See
    /// `select_sampling_handler` for the routing rule.
    per_call_sampling: PerCallSamplingHandlers,
}

#[derive(Debug, Clone, Default)]
struct HttpSessionState {
    session_id: Option<String>,
    protocol_version: Option<String>,
}

impl ProgressAwareHttpTransport {
    pub(crate) fn connect(
        config: &McpServerConnectionConfig,
        sampling_handler: Option<Arc<dyn SamplingHandler>>,
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

        Ok(Self {
            endpoint: endpoint.clone(),
            client,
            next_id: AtomicI64::new(1),
            next_progress_token: AtomicI64::new(1),
            capabilities: tokio::sync::Mutex::new(None),
            session: tokio::sync::RwLock::new(HttpSessionState::default()),
            sampling_handler,
            per_call_sampling: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        })
    }

    async fn initialize_if_needed(&self) -> Result<ServerCapabilities, McpTransportError> {
        let mut guard = self.capabilities.lock().await;
        if let Some(capabilities) = guard.clone() {
            return Ok(capabilities);
        }
        let capabilities = self.initialize().await?;
        *guard = Some(capabilities.clone());
        Ok(capabilities)
    }

    async fn initialize(&self) -> Result<ServerCapabilities, McpTransportError> {
        let request_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let response = self
            .post_message(
                JsonRpcMessage::Request(JsonRpcRequest::new(
                    JsonRpcId::Number(request_id),
                    "initialize".to_string(),
                    Some(initialize_params(json!({}), Value::Null)),
                )),
                false,
            )
            .await?;

        // Spec literal: `Mcp-Session-Id`. HeaderMap::get is
        // case-insensitive so this works regardless of how the server
        // capitalises the response header.
        let session_id = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        let body = self
            .decode_http_body(response, request_id, None)
            .await
            .map_err(|e| McpTransportError::ProtocolError(format!("initialize failed: {e}")))?;
        let result: InitializeResult = serde_json::from_value(body)?;

        {
            let mut session = self.session.write().await;
            session.session_id = session_id;
            session.protocol_version = Some(result.protocol_version.clone());
        }

        self.send_notification("notifications/initialized", Some(json!({})))
            .await?;

        Ok(result.capabilities)
    }

    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
        progress_registration: Option<(ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>)>,
    ) -> Result<Value, McpTransportError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.send_request_with_id(id, method, params, progress_registration)
            .await
    }

    /// Variant of `send_request` that uses a caller-allocated id so the
    /// cancellable call path can emit `notifications/cancelled` against
    /// the same in-flight request id.
    async fn send_request_with_id(
        &self,
        id: i64,
        method: &str,
        params: Option<Value>,
        progress_registration: Option<(ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>)>,
    ) -> Result<Value, McpTransportError> {
        let request = JsonRpcRequest::new(JsonRpcId::Number(id), method.to_string(), params);
        let response = self
            .post_message(JsonRpcMessage::Request(request), true)
            .await?;
        self.decode_http_body(response, id, progress_registration)
            .await
    }

    async fn send_initialized_request(
        &self,
        method: &str,
        params: Option<Value>,
        progress_registration: Option<(ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>)>,
    ) -> Result<Value, McpTransportError> {
        self.initialize_if_needed().await?;
        match self
            .send_request(method, params.clone(), progress_registration.clone())
            .await
        {
            Ok(value) => Ok(value),
            Err(McpTransportError::ProtocolError(message)) if message == "MCP session expired" => {
                self.reset_session().await;
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
    ) -> Result<reqwest::Response, McpTransportError> {
        let mut request = self.client.post(&self.endpoint).header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        );

        let session = self.session.read().await.clone();
        if let Some(protocol_version) = session.protocol_version {
            request = request.header("MCP-Protocol-Version", protocol_version);
        }
        if let Some(session_id) = session.session_id {
            // Spec literal is `Mcp-Session-Id` (title case), not `MCP-`.
            // HTTP headers are case-insensitive per RFC 7230 so both work
            // against compliant servers, but the spec writes it in title
            // case and some intermediaries are pickier than they should be.
            request = request.header("Mcp-Session-Id", session_id);
        }

        request = match message {
            JsonRpcMessage::Request(request_body) => request.json(&request_body),
            JsonRpcMessage::Notification(notification) => request.json(&notification),
            JsonRpcMessage::Response(response) => request.json(&response),
        };

        let response = request.send().await.map_err(|e| {
            McpTransportError::TransportError(format!("HTTP request failed: {}", e))
        })?;

        let status = response.status();
        if !status.is_success() {
            if status == reqwest::StatusCode::NOT_FOUND
                && self.session.read().await.session_id.is_some()
            {
                return Err(McpTransportError::ProtocolError(
                    "MCP session expired".to_string(),
                ));
            }

            let body = response.text().await.unwrap_or_default();
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

        Ok(response)
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

    async fn send_response_message(
        &self,
        response: JsonRpcResponse,
    ) -> Result<(), McpTransportError> {
        self.post_message(JsonRpcMessage::Response(response), false)
            .await?;
        Ok(())
    }

    async fn decode_http_body(
        &self,
        response: reqwest::Response,
        request_id: i64,
        progress_registration: Option<(ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>)>,
    ) -> Result<Value, McpTransportError> {
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();

        if content_type.starts_with("application/json") {
            let body: Value = response.json().await.map_err(|e| {
                McpTransportError::TransportError(format!("Failed to parse JSON response: {}", e))
            })?;
            return decode_http_response_payload(body, request_id, progress_registration);
        }

        if content_type.starts_with("text/event-stream") {
            return self
                .decode_sse_response(response, request_id, progress_registration)
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
        progress_registration: Option<(ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>)>,
    ) -> Result<Value, McpTransportError> {
        let progress_key = progress_registration.as_ref().map(|(key, _)| key.clone());
        let progress_tx = progress_registration.as_ref().map(|(_, tx)| tx.clone());
        let mut matched_response: Option<Result<Value, McpTransportError>> = None;
        let mut event_lines: Vec<String> = Vec::new();
        let mut line_buf: Vec<u8> = Vec::new();
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                McpTransportError::TransportError(format!(
                    "Failed to read SSE response body: {}",
                    e
                ))
            })?;

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
                        if let Some(result) = self
                            .process_sse_event(
                                &event_lines,
                                request_id,
                                progress_key.as_ref(),
                                progress_tx.as_ref(),
                            )
                            .await?
                        {
                            matched_response = Some(result);
                            break;
                        }
                        event_lines.clear();
                    } else {
                        event_lines.push(line);
                    }
                } else {
                    line_buf.push(byte);
                }
            }

            if matched_response.is_some() {
                break;
            }
        }

        if matched_response.is_none() && !event_lines.is_empty() {
            matched_response = self
                .process_sse_event(
                    &event_lines,
                    request_id,
                    progress_key.as_ref(),
                    progress_tx.as_ref(),
                )
                .await?;
        }

        matched_response.unwrap_or_else(|| {
            Err(McpTransportError::ProtocolError(format!(
                "Missing response for request id {}",
                request_id
            )))
        })
    }

    async fn process_sse_event(
        &self,
        lines: &[String],
        request_id: i64,
        progress_key: Option<&ProgressTokenKey>,
        progress_tx: Option<&mpsc::UnboundedSender<McpProgressUpdate>>,
    ) -> Result<Option<Result<Value, McpTransportError>>, McpTransportError> {
        let mut data_parts = Vec::new();
        for line in lines {
            if line.starts_with(':') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                data_parts.push(rest.trim_start().to_string());
            }
        }

        if data_parts.is_empty() {
            return Ok(None);
        }

        let payload = data_parts.join("\n");
        if payload.is_empty() {
            return Ok(None);
        }

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
                Ok(None)
            }
            JsonRpcMessage::Notification(notification) => {
                if let (Some(expected_key), Some(sender)) = (progress_key, progress_tx)
                    && let Some((key, update)) = decode_progress_notification(notification)
                    && key == *expected_key
                {
                    let _ = sender.send(update);
                }
                Ok(None)
            }
            JsonRpcMessage::Request(request) => {
                // HTTP per-request SSE stream: per spec 2025-06-18
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
                let response = handle_server_request(chosen.as_deref(), &request).await;
                self.send_response_message(response).await?;
                Ok(None)
            }
        }
    }

    async fn reset_session(&self) {
        *self.capabilities.lock().await = None;
        *self.session.write().await = HttpSessionState::default();
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
        progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
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

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        // Register the per-call sampling decision deterministically
        // BEFORE the request hits the wire. Awaiting the lock guarantees
        // the reader sees this entry before it can receive a
        // `sampling/createMessage` for our request id on the per-request
        // SSE stream.
        let _handler_guard =
            PerCallSamplingGuard::register(Arc::clone(&self.per_call_sampling), id, sampling).await;

        let request_fut = self.send_request_with_id(
            id,
            "tools/call",
            Some(serde_json::to_value(&params)?),
            progress_sender,
        );

        let result = match cancellation {
            None => request_fut.await,
            Some(token) => {
                tokio::pin!(request_fut);
                tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        // Per spec (2025-06-18 §Cancellation): SHOULD emit
                        // notifications/cancelled with the in-flight
                        // requestId so the server stops processing. The
                        // in-flight HTTP request is dropped by virtue of
                        // dropping `request_fut`; reqwest cancels the
                        // socket. Then we issue a separate POST with the
                        // cancellation notification.
                        let _ = self
                            .send_notification(
                                "notifications/cancelled",
                                Some(json!({
                                    "requestId": id,
                                    "reason": "client run cancelled",
                                })),
                            )
                            .await;
                        return Err(McpTransportError::TransportError(
                            CANCELLED_BY_CLIENT.to_string(),
                        ));
                    }
                    result = &mut request_fut => result,
                }
            }
        };

        let result = result?;
        let call_result: CallToolResult = serde_json::from_value(result)?;

        if call_result.is_error == Some(true) {
            return Err(McpTransportError::ServerError(tool_result_error_text(
                &call_result,
            )));
        }

        Ok(call_result)
    }

    fn transport_type(&self) -> TransportTypeId {
        TransportTypeId::Http
    }

    async fn server_capabilities(&self) -> Result<Option<ServerCapabilities>, McpTransportError> {
        Ok(Some(self.initialize_if_needed().await?))
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, McpTransportError> {
        self.send_initialized_request("resources/read", Some(json!({ "uri": uri })), None)
            .await
    }

    async fn close(&self) -> Result<(), McpTransportError> {
        // Per spec (2025-06-18 §Streamable HTTP / Session Management):
        // "Clients that no longer need a particular session SHOULD send an
        //  HTTP DELETE to the MCP endpoint with the Mcp-Session-Id header,
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
        let session_id = self.session.read().await.session_id.clone();
        if let Some(session_id) = session_id {
            self.send_session_termination(&session_id).await;
        }

        {
            let mut session = self.session.write().await;
            session.session_id = None;
            session.protocol_version = None;
        }

        {
            let mut capabilities = self.capabilities.lock().await;
            *capabilities = None;
        }

        Ok(())
    }

    async fn current_session_id(&self) -> Option<String> {
        self.session.read().await.session_id.clone()
    }
}

impl ProgressAwareHttpTransport {
    /// Best-effort `DELETE <endpoint>` with the session id header so the
    /// server can immediately free session state. Swallows all errors —
    /// the client has already decided to terminate, so a server 405 / 5xx
    /// / network error doesn't change the outcome locally.
    async fn send_session_termination(&self, session_id: &str) {
        let mut request = self
            .client
            .delete(&self.endpoint)
            .header("Mcp-Session-Id", session_id.to_string());
        let protocol_version = self.session.read().await.protocol_version.clone();
        if let Some(protocol_version) = protocol_version {
            request = request.header("MCP-Protocol-Version", protocol_version);
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
) -> Result<Arc<dyn McpToolTransport>, McpTransportError> {
    match config.transport {
        TransportTypeId::Stdio => {
            let transport = ProgressAwareStdioTransport::connect(config, sampling_handler).await?;
            Ok(Arc::new(transport))
        }
        TransportTypeId::Http => {
            let transport = ProgressAwareHttpTransport::connect(config, sampling_handler)?;
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
        tokio::sync::Mutex<HashMap<ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>>>,
    >,
    notification: JsonRpcNotification,
) {
    let Some((key, update)) = decode_progress_notification(notification) else {
        return;
    };
    let sender = subscribers.lock().await.get(&key).cloned();
    if let Some(sender) = sender
        && sender.send(update).is_err()
    {
        subscribers.lock().await.remove(&key);
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
    progress_registration: Option<(ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>)>,
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
                let _ = sender.send(update);
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
/// in-flight `tools/call`, so we use cardinality:
///   - 0 entries: nothing per-call to honor → fall back to transport-level
///     fixed handler (preserves legacy behaviour for callers that don't
///     wire a per-call factory).
///   - 1 entry: that entry's decision is authoritative — `Bound(h)` →
///     use h; `Denied` → reject (None). Never fall through on Denied,
///     since the factory was explicitly consulted for this call.
///   - More than one entry: ambiguous. Conservative — reject (None).
///     Operators who need sampling correctness with concurrent stdio
///     tool calls must serialize the calls. Falling through to the
///     transport fallback would leak across agents (the bug the
///     factory exists to prevent).
async fn select_sampling_handler(
    per_call: &PerCallSamplingHandlers,
    fallback: Option<&Arc<dyn SamplingHandler>>,
) -> Option<Arc<dyn SamplingHandler>> {
    let map = per_call.lock().await;
    match map.len() {
        0 => fallback.cloned(),
        1 => match map.values().next() {
            Some(PerCallSamplingEntry::Bound(h)) => Some(Arc::clone(h)),
            Some(PerCallSamplingEntry::Denied) => None,
            None => fallback.cloned(),
        },
        _ => None,
    }
}

pub(crate) async fn handle_server_request(
    sampling_handler: Option<&dyn SamplingHandler>,
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
    use std::time::Instant;

    use super::*;
    use mcp::CreateMessageResult;

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
        let response = handle_server_request(Some(&handler), &request).await;
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
        let response = handle_server_request(None, &request).await;
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
        let response = handle_server_request(Some(&handler), &request).await;
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
        let response = handle_server_request(Some(&handler), &request).await;
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
        let response = handle_server_request(None, &request).await;
        match response.payload {
            mcp::JsonRpcPayload::Error { error } => {
                assert!(error.to_string().contains("Method not supported"));
                assert!(error.to_string().contains("unknown/method"));
            }
            _ => panic!("expected error response"),
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
        let (tx, mut rx) = mpsc::unbounded_channel();
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
        let transport = ProgressAwareHttpTransport::connect(&cfg, None).unwrap();

        transport.close().await.unwrap();
        transport.close().await.unwrap();
    }

    /// Spec (2025-06-18, §Streamable HTTP / Session Management):
    /// "Clients that no longer need a particular session SHOULD send an
    ///  HTTP DELETE to the MCP endpoint with the Mcp-Session-Id header,
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
        let transport = ProgressAwareHttpTransport::connect(&cfg, None).unwrap();
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
        // spec literal is `Mcp-Session-Id`).
        let has_session_header = request.lines().any(|line| {
            line.to_ascii_lowercase().starts_with("mcp-session-id:")
                && line.contains("test-session-abc")
        });
        assert!(
            has_session_header,
            "DELETE must carry the Mcp-Session-Id header. Request was:\n{request}"
        );

        // Local state cleared after close().
        let session_after = transport.session.read().await.clone();
        assert!(session_after.session_id.is_none(), "session_id cleared");
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

    /// Spec (2025-06-18 §Cancellation): on a client-initiated cancel the
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

        let script = format!(
            r#"
while IFS= read -r LINE; do
    printf '%s\n' "$LINE" >> "{scratch}"
    case "$LINE" in
        *'"method":"initialize"'*)
            ID=$(printf '%s' "$LINE" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2024-11-05","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"mock","version":"0"}}}}}}\n' "$ID"
            ;;
    esac
done
"#,
            scratch = scratch_str
        );

        let mut cfg = mcp::transport::McpServerConnectionConfig::stdio(
            "stdio-cancel",
            "/bin/sh",
            vec!["-c".to_string(), script],
        );
        cfg.timeout_secs = 30;

        let transport = Arc::new(
            ProgressAwareStdioTransport::connect(&cfg, None)
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
    async fn select_sampling_routes_single_in_flight_to_per_call() {
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

        let chosen = select_sampling_handler(&per_call, Some(&fallback)).await;
        let chosen = chosen.expect("a handler was selected");

        // Invoke and verify which one handled it.
        let _ = chosen
            .handle_create_message(make_minimal_sampling_params())
            .await
            .expect("handler succeeded");
        assert_eq!(
            *last_caller.lock().await,
            Some("agent-A".to_string()),
            "single in-flight call -> per-call handler wins"
        );
    }

    #[tokio::test]
    async fn select_sampling_falls_back_when_zero_in_flight() {
        let last_caller = Arc::new(tokio::sync::Mutex::new(None::<String>));
        let per_call: PerCallSamplingHandlers = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let fallback: Arc<dyn SamplingHandler> = Arc::new(TaggedSamplingHandler {
            tag: "fallback".into(),
            last_caller: Arc::clone(&last_caller),
        });

        let chosen = select_sampling_handler(&per_call, Some(&fallback))
            .await
            .expect("fallback returned");
        let _ = chosen
            .handle_create_message(make_minimal_sampling_params())
            .await;
        assert_eq!(*last_caller.lock().await, Some("fallback".to_string()));
    }

    #[tokio::test]
    async fn select_sampling_returns_none_when_multiple_in_flight() {
        // With >1 in-flight calls we can't unambiguously route — return
        // None so the server gets method-not-supported. This is a
        // SECURITY fix: previously we fell back to the transport-level
        // fixed handler, which could be a different agent's executor.
        // The factory exists precisely to prevent that cross-agent
        // leak, so the conservative fix is to refuse rather than guess.
        let last_caller = Arc::new(tokio::sync::Mutex::new(None::<String>));
        let per_call: PerCallSamplingHandlers = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let agent_a: Arc<dyn SamplingHandler> = Arc::new(TaggedSamplingHandler {
            tag: "agent-A".into(),
            last_caller: Arc::clone(&last_caller),
        });
        let agent_b: Arc<dyn SamplingHandler> = Arc::new(TaggedSamplingHandler {
            tag: "agent-B".into(),
            last_caller: Arc::clone(&last_caller),
        });
        let fallback: Arc<dyn SamplingHandler> = Arc::new(TaggedSamplingHandler {
            tag: "fallback".into(),
            last_caller: Arc::clone(&last_caller),
        });
        per_call
            .lock()
            .await
            .insert(1, PerCallSamplingEntry::Bound(agent_a));
        per_call
            .lock()
            .await
            .insert(2, PerCallSamplingEntry::Bound(agent_b));

        assert!(
            select_sampling_handler(&per_call, Some(&fallback))
                .await
                .is_none(),
            "multiple in-flight => refuse rather than guess (no fallback)"
        );
    }

    #[tokio::test]
    async fn select_sampling_returns_none_on_denied_single_in_flight() {
        // Factory was consulted and explicitly refused this call. The
        // transport MUST NOT fall through to a transport-level fallback
        // handler — that would re-introduce the cross-agent leak the
        // factory exists to prevent.
        let last_caller = Arc::new(tokio::sync::Mutex::new(None::<String>));
        let per_call: PerCallSamplingHandlers = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let fallback: Arc<dyn SamplingHandler> = Arc::new(TaggedSamplingHandler {
            tag: "fallback".into(),
            last_caller: Arc::clone(&last_caller),
        });
        per_call
            .lock()
            .await
            .insert(7, PerCallSamplingEntry::Denied);
        assert!(
            select_sampling_handler(&per_call, Some(&fallback))
                .await
                .is_none(),
            "Denied entry must NEVER fall through to fallback"
        );
    }

    #[tokio::test]
    async fn select_sampling_returns_none_when_no_handlers_anywhere() {
        let per_call: PerCallSamplingHandlers = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        assert!(select_sampling_handler(&per_call, None).await.is_none());
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
        let transport = ProgressAwareHttpTransport::connect(&cfg, None).unwrap();
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

        let err = match ProgressAwareStdioTransport::connect(&cfg, None).await {
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
        let (tx, mut rx) = mpsc::unbounded_channel();
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
        let (tx, mut rx) = mpsc::unbounded_channel();
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
        let (tx, mut rx) = mpsc::unbounded_channel();
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
        let response = handle_server_request(Some(&handler), &request).await;
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
        let response = handle_server_request(Some(&handler), &request).await;
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
