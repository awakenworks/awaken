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

use crate::progress::McpProgressUpdate;
use crate::sampling::SamplingHandler;

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

    async fn close(&self) -> Result<(), McpTransportError> {
        Ok(())
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
    progress_subscribers: Arc<
        tokio::sync::Mutex<HashMap<ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>>>,
    >,
    next_id: AtomicI64,
    next_progress_token: AtomicI64,
    alive: Arc<AtomicBool>,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
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
        self.pending.lock().await.insert(id, tx);

        let progress_key = progress_registration.as_ref().map(|(key, _)| key.clone());
        if let Some((key, sender)) = progress_registration {
            self.progress_subscribers.lock().await.insert(key, sender);
        }

        if self.write_tx.send(WriteRequest { line }).await.is_err() {
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

        let session_id = response
            .headers()
            .get("MCP-Session-Id")
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
            request = request.header("MCP-Session-Id", session_id);
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
                let response =
                    handle_server_request(self.sampling_handler.as_deref(), &request).await;
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
        self.send_initialized_request("resources/read", Some(json!({ "uri": uri })), None)
            .await
    }

    async fn close(&self) -> Result<(), McpTransportError> {
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
