use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_mcp_wire::jsonrpc::ServerRequestError;
use mcp::transport::{InitializeResult, ServerCapabilities, ServerInfo, ServerToolCapabilities};
use mcp::{CallToolParams, CallToolResult, ListToolsParams, ListToolsResult, McpToolDefinition};
use serde_json::{Value, json};
use tokio::sync::Notify;

/// Protocol revisions this implementation can negotiate. Newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// A validated MCP tool invocation, independent of any runtime vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub struct McpCall {
    pub name: String,
    pub arguments: Value,
    pub meta: Option<Value>,
}

/// Host failures classified at the anti-corruption boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpHostError {
    InvalidArguments(String),
    NotFound(String),
    Rejected(String),
    Internal(String),
}

impl std::fmt::Display for McpHostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, message) = match self {
            Self::InvalidArguments(message) => ("invalid arguments", message),
            Self::NotFound(message) => ("not found", message),
            Self::Rejected(message) => ("rejected", message),
            Self::Internal(message) => ("internal", message),
        };
        write!(formatter, "{kind}: {message}")
    }
}

impl std::error::Error for McpHostError {}

/// Where a transport delivers server-to-client notifications for one request.
#[async_trait]
pub trait NotifySink: Send + Sync {
    async fn notify(&self, method: &str, params: Value);
}

/// A sink for requests with no notification channel.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

#[async_trait]
impl NotifySink for NullSink {
    async fn notify(&self, _method: &str, _params: Value) {}
}

/// The only business port the protocol core calls. `C` belongs entirely to the
/// caller: an open runtime may pass its adapter context, while a hosted flow may
/// pass principal/scope without either concept entering this crate.
#[async_trait]
pub trait McpToolHost<C: Sync>: Send + Sync {
    async fn list_tools(&self, context: &C) -> Result<Vec<McpToolDefinition>, McpHostError>;

    async fn call_tool(
        &self,
        context: &C,
        call: McpCall,
        notifications: &dyn NotifySink,
    ) -> Result<CallToolResult, McpHostError>;

    /// Client notifications are best-effort protocol signals. Hosts override only
    /// when they have application state to update.
    async fn notification(&self, context: &C, method: &str, params: &Value) {
        let _ = (context, method, params);
    }
}

#[async_trait]
impl<C, H> McpToolHost<C> for Arc<H>
where
    C: Send + Sync,
    H: McpToolHost<C> + ?Sized,
{
    async fn list_tools(&self, context: &C) -> Result<Vec<McpToolDefinition>, McpHostError> {
        (**self).list_tools(context).await
    }

    async fn call_tool(
        &self,
        context: &C,
        call: McpCall,
        notifications: &dyn NotifySink,
    ) -> Result<CallToolResult, McpHostError> {
        (**self).call_tool(context, call, notifications).await
    }

    async fn notification(&self, context: &C, method: &str, params: &Value) {
        (**self).notification(context, method, params).await;
    }
}

struct CancellationSignal {
    cancelled: AtomicBool,
    notified: Notify,
}

impl CancellationSignal {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notified: Notify::new(),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notified.notify_waiters();
    }

    async fn cancelled(&self) {
        if self.cancelled.load(Ordering::Acquire) {
            return;
        }
        self.notified.notified().await;
    }
}

/// Transport-neutral MCP server over a caller-supplied host port and context.
pub struct McpServer<H, C> {
    name: String,
    version: String,
    host: H,
    active: Mutex<HashMap<String, Arc<CancellationSignal>>>,
    _context: PhantomData<fn(&C)>,
}

impl<H, C> McpServer<H, C>
where
    H: McpToolHost<C>,
    C: Send + Sync,
{
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>, host: H) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            host,
            active: Mutex::new(HashMap::new()),
            _context: PhantomData,
        }
    }

    /// The caller-owned host, for adapter-specific lifecycle wiring only.
    pub fn host(&self) -> &H {
        &self.host
    }

    /// Dispatch without an envelope request id (compatibility/stdio path).
    pub async fn handle(
        &self,
        context: &C,
        method: &str,
        params: Value,
        sink: &dyn NotifySink,
    ) -> Result<Value, ServerRequestError> {
        self.handle_request(context, None, method, params, sink)
            .await
    }

    /// Dispatch with the JSON-RPC request id available for cancellation.
    pub async fn handle_request(
        &self,
        context: &C,
        request_id: Option<&Value>,
        method: &str,
        params: Value,
        sink: &dyn NotifySink,
    ) -> Result<Value, ServerRequestError> {
        match method {
            "initialize" => self.initialize(&params),
            "ping" => Ok(json!({})),
            "tools/list" => self.list_tools(context, params).await,
            "tools/call" => self.call_tool(context, request_id, params, sink).await,
            _ => Err(ServerRequestError::method_not_found(method)),
        }
    }

    /// Handle a client notification. Cancellation is consumed by the core before
    /// the application hook, so a cancelled request cannot later produce a result.
    pub async fn handle_notification(&self, context: &C, method: &str, params: &Value) {
        if method == "notifications/cancelled"
            && let Some(id) = params.get("requestId")
            && let Some(signal) = self
                .active
                .lock()
                .expect("MCP cancellation registry")
                .get(&request_key(id))
                .cloned()
        {
            signal.cancel();
        }
        self.host.notification(context, method, params).await;
    }

    fn initialize(&self, params: &Value) -> Result<Value, ServerRequestError> {
        let requested = params
            .as_object()
            .and_then(|params| params.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or(mcp::MCP_PROTOCOL_VERSION);
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
            return Err(ServerRequestError::invalid_params(format!(
                "unsupported MCP protocol version: {requested}"
            )));
        }
        let result = InitializeResult {
            protocol_version: requested.to_string(),
            capabilities: ServerCapabilities {
                tools: Some(ServerToolCapabilities {
                    list_changed: Some(true),
                }),
                ..ServerCapabilities::default()
            },
            server_info: ServerInfo::new(self.name.clone(), self.version.clone()),
        };
        serde_json::to_value(result).map_err(|e| ServerRequestError::internal(e.to_string()))
    }

    async fn list_tools(&self, context: &C, params: Value) -> Result<Value, ServerRequestError> {
        if !params.is_null() {
            serde_json::from_value::<ListToolsParams>(params)
                .map_err(|e| ServerRequestError::invalid_params(format!("tools/list: {e}")))?;
        }
        let tools = self
            .host
            .list_tools(context)
            .await
            .map_err(map_host_error)?;
        serde_json::to_value(ListToolsResult {
            tools,
            next_cursor: None,
        })
        .map_err(|e| ServerRequestError::internal(e.to_string()))
    }

    async fn call_tool(
        &self,
        context: &C,
        request_id: Option<&Value>,
        params: Value,
        sink: &dyn NotifySink,
    ) -> Result<Value, ServerRequestError> {
        let params: CallToolParams = serde_json::from_value(params)
            .map_err(|e| ServerRequestError::invalid_params(format!("tools/call: {e}")))?;
        let call = McpCall {
            name: params.name,
            arguments: params.arguments.unwrap_or_else(|| json!({})),
            meta: params.meta,
        };

        let Some(id) = request_id else {
            return self
                .host
                .call_tool(context, call, sink)
                .await
                .map_err(map_host_error)
                .and_then(serialize_call_result);
        };
        let key = request_key(id);
        let signal = Arc::new(CancellationSignal::new());
        self.active
            .lock()
            .expect("MCP cancellation registry")
            .insert(key.clone(), signal.clone());
        let outcome = tokio::select! {
            result = self.host.call_tool(context, call, sink) => {
                result.map_err(map_host_error).and_then(serialize_call_result)
            }
            () = signal.cancelled() => {
                Err(ServerRequestError::internal("request cancelled"))
            }
        };
        self.active
            .lock()
            .expect("MCP cancellation registry")
            .remove(&key);
        outcome
    }
}

fn serialize_call_result(result: CallToolResult) -> Result<Value, ServerRequestError> {
    serde_json::to_value(result).map_err(|e| ServerRequestError::internal(e.to_string()))
}

fn map_host_error(error: McpHostError) -> ServerRequestError {
    match error {
        McpHostError::InvalidArguments(message) | McpHostError::NotFound(message) => {
            ServerRequestError::invalid_params(message)
        }
        McpHostError::Rejected(message) | McpHostError::Internal(message) => {
            ServerRequestError::internal(message)
        }
    }
}

fn request_key(id: &Value) -> String {
    id.to_string()
}

/// Wrap exactly one service outcome in exactly one JSON-RPC response envelope.
#[must_use]
pub fn jsonrpc_reply(id: Value, outcome: Result<Value, ServerRequestError>) -> Value {
    match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(err) => json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": err.code, "message": err.message },
        }),
    }
}

/// Canonical JSON-RPC notification envelope for an export-set change.
#[must_use]
pub fn tools_list_changed_notification() -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/tools/list_changed",
        "params": {},
    })
}

/// Emit an export-set change through any transport notification sink.
pub async fn notify_tools_list_changed(sink: &dyn NotifySink) {
    sink.notify("notifications/tools/list_changed", json!({}))
        .await;
}
