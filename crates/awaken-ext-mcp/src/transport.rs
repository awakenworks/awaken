//! MCP tool transport: wraps MCP tool calls as awaken `Tool` implementations.
//!
//! Contains the `McpToolTransport` trait (raw MCP client abstraction) and
//! `McpTool` which adapts an MCP tool definition into an awaken `Tool`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
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

use crate::progress::McpProgressUpdate;
use crate::sampling::SamplingHandler;

type PendingRequestSender = oneshot::Sender<Result<Value, McpTransportError>>;
type PendingRequests = Arc<Mutex<HashMap<i64, PendingRequestSender>>>;

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
    ) -> Result<CallToolResult, McpTransportError>;

    fn transport_type(&self) -> TransportTypeId;

    async fn read_resource(&self, _uri: &str) -> Result<Value, McpTransportError> {
        Err(McpTransportError::TransportError(
            "read_resource not supported".to_string(),
        ))
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
}

// ── Stdio transport ──

pub(crate) struct ProgressAwareStdioTransport {
    write_tx: mpsc::Sender<WriteRequest>,
    pending: PendingRequests,
    progress_subscribers:
        Arc<Mutex<HashMap<ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>>>>,
    next_id: AtomicI64,
    next_progress_token: AtomicI64,
    alive: Arc<AtomicBool>,
    _child: Arc<tokio::sync::Mutex<Child>>,
    timeout: Duration,
    capabilities: Option<ServerCapabilities>,
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
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
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
        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let progress_subscribers: Arc<
            Mutex<HashMap<ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>>>,
        > = Arc::new(Mutex::new(HashMap::new()));

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
            }
        });

        let pending_reader = Arc::clone(&pending);
        let progress_reader = Arc::clone(&progress_subscribers);
        let alive_reader = Arc::clone(&alive);
        let write_tx_reader = write_tx.clone();
        let sampling_handler_reader = sampling_handler.clone();
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
                                let tx = pending_reader.lock().unwrap().remove(&id);
                                if let Some(tx) = tx {
                                    let result = map_response_payload(response.payload);
                                    let _ = tx.send(result);
                                }
                            }
                        }
                        Ok(JsonRpcMessage::Notification(notification)) => {
                            handle_progress_notification(&progress_reader, notification);
                        }
                        Ok(JsonRpcMessage::Request(request)) => {
                            let handler = sampling_handler_reader.clone();
                            let wtx = write_tx_reader.clone();
                            tokio::spawn(async move {
                                let response =
                                    handle_server_request(handler.as_deref(), &request).await;
                                let line = format!(
                                    "{}\n",
                                    serde_json::to_string(&response).unwrap_or_default()
                                );
                                let _ = wtx.send(WriteRequest { line }).await;
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

            pending_reader.lock().unwrap().clear();
            progress_reader.lock().unwrap().clear();
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
            _child: Arc::new(tokio::sync::Mutex::new(child)),
            timeout: Duration::from_secs(config.timeout_secs),
            capabilities: None,
        };

        let mut capabilities = InitializeCapabilities::default();
        if sampling_handler.is_some() {
            capabilities.sampling = Some(SamplingCapabilities::default());
        }
        let init_result: InitializeResult = serde_json::from_value(
            transport
                .send_request(
                    "initialize",
                    Some(initialize_params(
                        serde_json::to_value(&capabilities).unwrap_or_else(|_| json!({})),
                        config.config.clone(),
                    )),
                    None,
                )
                .await?,
        )?;
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
            .send(WriteRequest { line })
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
        if !self.alive.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = JsonRpcRequest::new(JsonRpcId::Number(id), method.to_string(), params);
        let line = format!("{}\n", serde_json::to_string(&request)?);

        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let progress_key = progress_registration.as_ref().map(|(key, _)| key.clone());
        if let Some((key, sender)) = progress_registration {
            self.progress_subscribers
                .lock()
                .unwrap()
                .insert(key, sender);
        }

        if self.write_tx.send(WriteRequest { line }).await.is_err() {
            self.pending.lock().unwrap().remove(&id);
            if let Some(key) = progress_key {
                self.progress_subscribers.lock().unwrap().remove(&key);
            }
            return Err(McpTransportError::ConnectionClosed);
        }

        let response = tokio::time::timeout(self.timeout, rx).await;
        if let Some(key) = progress_key {
            self.progress_subscribers.lock().unwrap().remove(&key);
        }

        match response {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.pending.lock().unwrap().remove(&id);
                Err(McpTransportError::ConnectionClosed)
            }
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(McpTransportError::Timeout(format!(
                    "Request timed out after {:?}",
                    self.timeout
                )))
            }
        }
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
    ) -> Result<CallToolResult, McpTransportError> {
        let progress_registration = progress_tx.map(|sender| {
            let token =
                ProgressToken::Number(self.next_progress_token.fetch_add(1, Ordering::SeqCst));
            let key = ProgressTokenKey::from(&token);
            (token, key, sender)
        });

        let (meta, progress_sender) = if let Some((token, key, sender)) = progress_registration {
            let mut map = Map::new();
            map.insert("progressToken".to_string(), serde_json::to_value(token)?);
            (Some(Value::Object(map)), Some((key, sender)))
        } else {
            (None, None)
        };

        let params = CallToolParams {
            name: name.to_string(),
            arguments: Some(args),
            task: None,
            meta,
        };

        let result = self
            .send_request(
                "tools/call",
                Some(serde_json::to_value(&params)?),
                progress_sender,
            )
            .await?;
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
        self.send_request("resources/read", Some(json!({"uri": uri})), None)
            .await
    }
}

// ── HTTP transport ──

pub(crate) struct ProgressAwareHttpTransport {
    endpoint: String,
    client: reqwest::Client,
    next_id: AtomicI64,
    next_progress_token: AtomicI64,
    capabilities: tokio::sync::Mutex<Option<ServerCapabilities>>,
}

impl ProgressAwareHttpTransport {
    pub(crate) fn connect(config: &McpServerConnectionConfig) -> Result<Self, McpTransportError> {
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
        let result: InitializeResult = serde_json::from_value(
            self.send_request(
                "initialize",
                Some(initialize_params(json!({}), Value::Null)),
                None,
            )
            .await?,
        )?;
        Ok(result.capabilities)
    }

    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
        progress_registration: Option<(ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>)>,
    ) -> Result<Value, McpTransportError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = JsonRpcRequest::new(JsonRpcId::Number(id), method.to_string(), params);

        let response = self
            .client
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                McpTransportError::TransportError(format!("HTTP request failed: {}", e))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(McpTransportError::TransportError(format!(
                "HTTP error: {} - {}",
                status, body
            )));
        }

        let body: Value = response.json().await.map_err(|e| {
            McpTransportError::TransportError(format!("Failed to parse JSON response: {}", e))
        })?;
        decode_http_response_payload(body, id, progress_registration)
    }

    async fn send_initialized_request(
        &self,
        method: &str,
        params: Option<Value>,
        progress_registration: Option<(ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>)>,
    ) -> Result<Value, McpTransportError> {
        self.initialize_if_needed().await?;
        self.send_request(method, params, progress_registration)
            .await
    }
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
    ) -> Result<CallToolResult, McpTransportError> {
        let progress_registration = progress_tx.map(|sender| {
            let token =
                ProgressToken::Number(self.next_progress_token.fetch_add(1, Ordering::SeqCst));
            let key = ProgressTokenKey::from(&token);
            (token, key, sender)
        });

        let (meta, progress_sender) = if let Some((token, key, sender)) = progress_registration {
            let mut map = Map::new();
            map.insert("progressToken".to_string(), serde_json::to_value(token)?);
            (Some(Value::Object(map)), Some((key, sender)))
        } else {
            (None, None)
        };

        let params = CallToolParams {
            name: name.to_string(),
            arguments: Some(args),
            task: None,
            meta,
        };

        let result = self
            .send_initialized_request(
                "tools/call",
                Some(serde_json::to_value(&params)?),
                progress_sender,
            )
            .await?;
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
        self.send_initialized_request("resources/read", Some(json!({"uri": uri})), None)
            .await
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
            let transport = ProgressAwareHttpTransport::connect(config)?;
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

fn handle_progress_notification(
    subscribers: &Arc<Mutex<HashMap<ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>>>>,
    notification: JsonRpcNotification,
) {
    let Some((key, update)) = decode_progress_notification(notification) else {
        return;
    };
    let sender = subscribers.lock().unwrap().get(&key).cloned();
    if let Some(sender) = sender
        && sender.send(update).is_err()
    {
        subscribers.lock().unwrap().remove(&key);
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
}
