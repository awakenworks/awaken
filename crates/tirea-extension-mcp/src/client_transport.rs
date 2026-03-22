use async_trait::async_trait;
use mcp::transport::{
    ClientInfo, InitializeCapabilities, InitializeResult, McpServerConnectionConfig,
    McpTransportError, SamplingCapabilities, ServerCapabilities, TransportTypeId,
};
use mcp::{
    CallToolParams, CallToolResult, CreateMessageParams, CreateMessageResult, JsonRpcId,
    JsonRpcMessage, JsonRpcNotification, JsonRpcPayload, JsonRpcRequest, JsonRpcResponse,
    ListToolsResult, McpToolDefinition, ProgressNotificationParams, ProgressToken,
    MCP_PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

type PendingRequestSender = oneshot::Sender<Result<Value, McpTransportError>>;
type PendingRequests = Arc<Mutex<HashMap<i64, PendingRequestSender>>>;

#[derive(Debug, Clone)]
pub struct McpProgressUpdate {
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<String>,
}

/// Handler for MCP `sampling/createMessage` requests from the server.
///
/// When an MCP server sends a `sampling/createMessage` request during tool
/// execution, this handler is invoked to route it to an LLM for inference.
#[async_trait]
pub trait SamplingHandler: Send + Sync {
    async fn handle_create_message(
        &self,
        params: CreateMessageParams,
    ) -> Result<CreateMessageResult, McpTransportError>;
}

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

    /// Read a resource by URI (e.g. `ui://...` for MCP Apps).
    ///
    /// Returns the raw JSON value from the `resources/read` response.
    async fn read_resource(&self, _uri: &str) -> Result<Value, McpTransportError> {
        Err(McpTransportError::TransportError(
            "read_resource not supported".to_string(),
        ))
    }
}

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
            // HTTP transport is request-response; no persistent bidirectional
            // channel for server-initiated requests like sampling.
            let transport = ProgressAwareHttpTransport::connect(config)?;
            Ok(Arc::new(transport))
        }
    }
}

struct WriteRequest {
    line: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
enum ProgressTokenKey {
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

fn handle_progress_notification(
    subscribers: &Arc<Mutex<HashMap<ProgressTokenKey, mpsc::UnboundedSender<McpProgressUpdate>>>>,
    notification: JsonRpcNotification,
) {
    let Some((key, update)) = decode_progress_notification(notification) else {
        return;
    };
    let sender = subscribers.lock().unwrap().get(&key).cloned();
    if let Some(sender) = sender {
        if sender.send(update).is_err() {
            subscribers.lock().unwrap().remove(&key);
        }
    }
}

fn initialize_params(capabilities: Value, config: Value) -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": capabilities,
        "clientInfo": serde_json::to_value(ClientInfo::new(
            "tirea-mcp",
            env!("CARGO_PKG_VERSION"),
        )).unwrap_or_else(|_| json!({})),
        "config": config,
    })
}

fn decode_progress_notification(
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

fn tool_result_error_text(result: &CallToolResult) -> String {
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

fn map_response_payload(payload: JsonRpcPayload) -> Result<Value, McpTransportError> {
    match payload {
        JsonRpcPayload::Success { result } => Ok(result),
        JsonRpcPayload::Error { error } => Err(McpTransportError::ServerError(format!(
            "MCP Error: {}",
            error
        ))),
    }
}

fn parse_json_rpc_message(value: Value) -> Result<JsonRpcMessage, McpTransportError> {
    match serde_json::from_value::<JsonRpcMessage>(value.clone()) {
        Ok(message) => Ok(message),
        Err(_) => serde_json::from_value::<JsonRpcResponse>(value)
            .map(JsonRpcMessage::Response)
            .map_err(McpTransportError::from),
    }
}

fn decode_http_response_payload(
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

async fn handle_server_request(
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[derive(Clone)]
    struct HttpResponseSpec {
        status: u16,
        content_type: &'static str,
        body: String,
    }

    impl HttpResponseSpec {
        fn json(body: Value) -> Self {
            Self {
                status: 200,
                content_type: "application/json",
                body: body.to_string(),
            }
        }

        fn text(status: u16, body: impl Into<String>) -> Self {
            Self {
                status,
                content_type: "text/plain",
                body: body.into(),
            }
        }
    }

    fn status_text(status: u16) -> &'static str {
        match status {
            200 => "OK",
            400 => "Bad Request",
            500 => "Internal Server Error",
            _ => "OK",
        }
    }

    fn header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
    }

    fn content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|line| {
                let (k, v) = line.split_once(':')?;
                if k.trim().eq_ignore_ascii_case("content-length") {
                    v.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0)
    }

    async fn read_json_body(stream: &mut TcpStream) -> Option<Value> {
        let mut buf = Vec::new();
        let mut chunk = [0_u8; 1024];
        let (header_end, body_len) = loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            let Some(end) = header_end(&buf) else {
                continue;
            };
            let headers = std::str::from_utf8(&buf[..end]).ok()?;
            let len = content_length(headers);
            break (end, len);
        };

        while buf.len() < header_end + body_len {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }

        serde_json::from_slice(&buf[header_end..header_end + body_len]).ok()
    }

    async fn spawn_http_server(
        handler: Arc<dyn Fn(Value) -> HttpResponseSpec + Send + Sync>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind http listener");
        let addr = listener.local_addr().expect("listener addr");
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let Some(request_body) = read_json_body(&mut stream).await else {
                        return;
                    };
                    let response = handler(request_body);
                    let payload = response.body;
                    let head = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response.status,
                        status_text(response.status),
                        response.content_type,
                        payload.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(payload.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://{}", addr), handle)
    }

    fn tool_call_success_response(request: &Value, text: &str) -> HttpResponseSpec {
        HttpResponseSpec::json(json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "result": {
                "content": [{"type": "text", "text": text}]
            }
        }))
    }

    fn initialize_response(request: &Value, capabilities: Value) -> HttpResponseSpec {
        HttpResponseSpec::json(json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "result": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": capabilities,
                "serverInfo": {
                    "name": "test-server",
                    "version": "1.0.0"
                }
            }
        }))
    }

    #[tokio::test]
    async fn http_call_tool_sets_progress_token_meta_conditionally() {
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let requests_handler = Arc::clone(&requests);
        let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
            requests_handler
                .lock()
                .expect("requests lock")
                .push(request.clone());
            match request["method"].as_str().unwrap_or_default() {
                "initialize" => initialize_response(&request, json!({})),
                "tools/call" => tool_call_success_response(&request, "ok"),
                other => panic!("unexpected method: {other}"),
            }
        }))
        .await;

        let cfg = McpServerConnectionConfig::http("http_progress_meta", endpoint);
        let transport = ProgressAwareHttpTransport::connect(&cfg).expect("connect transport");

        let (progress_tx, _progress_rx) = mpsc::unbounded_channel();
        let _ = transport
            .call_tool("echo", json!({"message":"hello"}), Some(progress_tx))
            .await
            .expect("tool call with progress");
        let _ = transport
            .call_tool("echo", json!({"message":"hello"}), None)
            .await
            .expect("tool call without progress");

        server.abort();
        let captured = requests.lock().expect("requests lock");
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[0]["method"], json!("initialize"));
        assert_eq!(captured[1]["method"], json!("tools/call"));
        assert!(captured[1]["params"]["_meta"]["progressToken"].is_number());
        assert!(captured[2]["params"].get("_meta").is_none());
    }

    #[tokio::test]
    async fn http_server_capabilities_are_initialized_once_and_cached() {
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let requests_handler = Arc::clone(&requests);
        let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
            requests_handler
                .lock()
                .expect("requests lock")
                .push(request.clone());
            match request["method"].as_str().unwrap_or_default() {
                "initialize" => initialize_response(
                    &request,
                    json!({
                        "prompts": {},
                        "resources": {}
                    }),
                ),
                other => panic!("unexpected method: {other}"),
            }
        }))
        .await;

        let cfg = McpServerConnectionConfig::http("http_caps", endpoint);
        let transport = ProgressAwareHttpTransport::connect(&cfg).expect("connect transport");

        let first = transport
            .server_capabilities()
            .await
            .expect("server capabilities")
            .expect("capabilities");
        let second = transport
            .server_capabilities()
            .await
            .expect("server capabilities")
            .expect("capabilities");

        server.abort();
        assert!(first.prompts.is_some());
        assert!(first.resources.is_some());
        assert!(second.prompts.is_some());
        assert!(second.resources.is_some());

        let captured = requests.lock().expect("requests lock");
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0]["method"], json!("initialize"));
    }

    #[tokio::test]
    async fn http_non_success_status_is_reported() {
        let (endpoint, server) =
            spawn_http_server(Arc::new(|_| HttpResponseSpec::text(500, "upstream error"))).await;
        let cfg = McpServerConnectionConfig::http("http_error_status", endpoint);
        let transport = ProgressAwareHttpTransport::connect(&cfg).expect("connect transport");
        let err = transport.list_tools().await.expect_err("error");
        server.abort();
        assert!(matches!(err, McpTransportError::TransportError(_)));
        assert!(err.to_string().contains("HTTP error"));
    }

    #[tokio::test]
    async fn http_invalid_json_response_is_reported() {
        let (endpoint, server) =
            spawn_http_server(Arc::new(|_| HttpResponseSpec::text(200, "not-json"))).await;
        let cfg = McpServerConnectionConfig::http("http_invalid_json", endpoint);
        let transport = ProgressAwareHttpTransport::connect(&cfg).expect("connect transport");
        let err = transport.list_tools().await.expect_err("error");
        server.abort();
        assert!(matches!(err, McpTransportError::TransportError(_)));
        assert!(err.to_string().contains("Failed to parse JSON response"));
    }

    #[tokio::test]
    async fn http_json_rpc_error_payload_is_reported() {
        let (endpoint, server) = spawn_http_server(Arc::new(|request| {
            HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "error": {"code": -32000, "message": "rpc failed"}
            }))
        }))
        .await;
        let cfg = McpServerConnectionConfig::http("http_rpc_error", endpoint);
        let transport = ProgressAwareHttpTransport::connect(&cfg).expect("connect transport");
        let err = transport.list_tools().await.expect_err("error");
        server.abort();
        assert!(matches!(err, McpTransportError::ServerError(_)));
        assert!(err.to_string().contains("rpc failed"));
    }

    #[tokio::test]
    async fn http_call_tool_with_is_error_result_returns_server_error() {
        let (endpoint, server) = spawn_http_server(Arc::new(|request| {
            match request["method"].as_str().unwrap_or_default() {
                "initialize" => initialize_response(&request, json!({})),
                "tools/call" => HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": {
                        "content": [{"type": "text", "text": "tool failed"}],
                        "isError": true
                    }
                })),
                other => panic!("unexpected method: {other}"),
            }
        }))
        .await;
        let cfg = McpServerConnectionConfig::http("http_tool_error", endpoint);
        let transport = ProgressAwareHttpTransport::connect(&cfg).expect("connect transport");
        let err = transport
            .call_tool("echo", json!({"message":"x"}), None)
            .await
            .expect_err("server error");
        server.abort();
        assert!(matches!(err, McpTransportError::ServerError(_)));
        assert!(err.to_string().contains("tool failed"));
    }

    #[tokio::test]
    async fn http_call_tool_preserves_structured_content() {
        let (endpoint, server) = spawn_http_server(Arc::new(|request| {
            match request["method"].as_str().unwrap_or_default() {
                "initialize" => initialize_response(&request, json!({})),
                "tools/call" => HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": {
                        "content": [{"type": "text", "text": "sum complete"}],
                        "structuredContent": {"sum": 3, "values": [1, 2]}
                    }
                })),
                other => panic!("unexpected method: {other}"),
            }
        }))
        .await;
        let cfg = McpServerConnectionConfig::http("http_structured", endpoint);
        let transport = ProgressAwareHttpTransport::connect(&cfg).expect("connect transport");
        let result = transport
            .call_tool("sum", json!({"values":[1,2]}), None)
            .await
            .expect("structured tool result");
        server.abort();

        assert_eq!(result.content.len(), 1);
        assert_eq!(result.content[0].as_text(), Some("sum complete"));
        assert_eq!(
            result.structured_content,
            Some(json!({"sum": 3, "values": [1, 2]}))
        );
    }

    #[tokio::test]
    async fn http_list_prompts_parses_prompt_definitions() {
        let (endpoint, server) = spawn_http_server(Arc::new(|request| {
            match request["method"].as_str().unwrap_or_default() {
                "initialize" => initialize_response(&request, json!({"prompts": {}})),
                "prompts/list" => HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": {
                        "prompts": [{
                            "name": "review",
                            "title": "Review",
                            "description": "Review code",
                            "arguments": [{
                                "name": "path",
                                "description": "Target path",
                                "required": true
                            }]
                        }]
                    }
                })),
                other => panic!("unexpected method: {other}"),
            }
        }))
        .await;
        let cfg = McpServerConnectionConfig::http("http_prompts", endpoint);
        let transport = ProgressAwareHttpTransport::connect(&cfg).expect("connect transport");
        let prompts = transport.list_prompts().await.expect("prompt list");
        server.abort();

        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "review");
        assert_eq!(prompts[0].arguments.len(), 1);
        assert!(prompts[0].arguments[0].required);
    }

    #[tokio::test]
    async fn http_get_prompt_sends_arguments_and_parses_messages() {
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let requests_handler = Arc::clone(&requests);
        let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
            requests_handler
                .lock()
                .expect("requests lock")
                .push(request.clone());
            match request["method"].as_str().unwrap_or_default() {
                "initialize" => initialize_response(&request, json!({"prompts": {}})),
                "prompts/get" => HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": {
                        "description": "Review prompt",
                        "messages": [{
                            "role": "user",
                            "content": {"type": "text", "text": "Review src/lib.rs"}
                        }]
                    }
                })),
                other => panic!("unexpected method: {other}"),
            }
        }))
        .await;
        let cfg = McpServerConnectionConfig::http("http_get_prompt", endpoint);
        let transport = ProgressAwareHttpTransport::connect(&cfg).expect("connect transport");
        let prompt = transport
            .get_prompt(
                "review",
                Some(HashMap::from([(
                    "path".to_string(),
                    "src/lib.rs".to_string(),
                )])),
            )
            .await
            .expect("prompt result");
        server.abort();

        assert_eq!(prompt.description.as_deref(), Some("Review prompt"));
        assert_eq!(prompt.messages.len(), 1);
        assert_eq!(prompt.messages[0].role, "user");
        assert_eq!(
            prompt.messages[0].content["text"],
            json!("Review src/lib.rs")
        );

        let captured = requests.lock().expect("requests lock");
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0]["method"], json!("initialize"));
        assert_eq!(captured[1]["method"], json!("prompts/get"));
        assert_eq!(captured[1]["params"]["name"], json!("review"));
        assert_eq!(
            captured[1]["params"]["arguments"]["path"],
            json!("src/lib.rs")
        );
    }

    #[tokio::test]
    async fn http_list_resources_parses_resource_definitions() {
        let (endpoint, server) = spawn_http_server(Arc::new(|request| {
            match request["method"].as_str().unwrap_or_default() {
                "initialize" => initialize_response(&request, json!({"resources": {}})),
                "resources/list" => HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": {
                        "resources": [{
                            "uri": "file://guide.md",
                            "name": "guide",
                            "title": "Guide",
                            "description": "Guide doc",
                            "mimeType": "text/markdown",
                            "size": 42
                        }]
                    }
                })),
                other => panic!("unexpected method: {other}"),
            }
        }))
        .await;
        let cfg = McpServerConnectionConfig::http("http_resources", endpoint);
        let transport = ProgressAwareHttpTransport::connect(&cfg).expect("connect transport");
        let resources = transport.list_resources().await.expect("resource list");
        server.abort();

        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].uri, "file://guide.md");
        assert_eq!(resources[0].mime_type.as_deref(), Some("text/markdown"));
        assert_eq!(resources[0].size, Some(42));
    }

    #[tokio::test]
    async fn http_read_resource_auto_initializes_before_read() {
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let requests_handler = Arc::clone(&requests);
        let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
            requests_handler
                .lock()
                .expect("requests lock")
                .push(request.clone());
            match request["method"].as_str().unwrap_or_default() {
                "initialize" => initialize_response(&request, json!({"resources": {}})),
                "resources/read" => HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": {
                        "contents": [{
                            "uri": "file://guide.md",
                            "text": "# Guide",
                            "mimeType": "text/markdown"
                        }]
                    }
                })),
                other => panic!("unexpected method: {other}"),
            }
        }))
        .await;

        let cfg = McpServerConnectionConfig::http("http_read_resource", endpoint);
        let transport = ProgressAwareHttpTransport::connect(&cfg).expect("connect transport");
        let resource = transport
            .read_resource("file://guide.md")
            .await
            .expect("resource");
        server.abort();

        assert_eq!(resource["contents"][0]["text"], json!("# Guide"));

        let captured = requests.lock().expect("requests lock");
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0]["method"], json!("initialize"));
        assert_eq!(captured[1]["method"], json!("resources/read"));
        assert_eq!(captured[1]["params"]["uri"], json!("file://guide.md"));
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

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_http_tool_calls_route_progress_by_token() {
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let requests_handler = Arc::clone(&requests);
        let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
            requests_handler
                .lock()
                .expect("requests lock")
                .push(request.clone());
            match request["method"].as_str().unwrap_or_default() {
                "initialize" => initialize_response(&request, json!({})),
                "tools/call" => {
                    let token = request["params"]["_meta"]["progressToken"].clone();
                    let label = request["params"]["arguments"]["label"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    HttpResponseSpec::json(json!([
                        {
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": token,
                                "progress": 1.0,
                                "total": 1.0,
                                "message": label
                            }
                        },
                        {
                            "jsonrpc": "2.0",
                            "id": request["id"].clone(),
                            "result": {
                                "content": [{"type": "text", "text": request["params"]["arguments"]["label"].clone()}]
                            }
                        }
                    ]))
                }
                other => panic!("unexpected method: {other}"),
            }
        }))
        .await;

        let cfg = McpServerConnectionConfig::http("http_concurrent", endpoint);
        let transport =
            Arc::new(ProgressAwareHttpTransport::connect(&cfg).expect("connect transport"));

        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let transport_a = Arc::clone(&transport);
        let transport_b = Arc::clone(&transport);

        let call_a = tokio::spawn(async move {
            transport_a
                .call_tool("echo", json!({"label":"A"}), Some(tx_a))
                .await
        });
        let call_b = tokio::spawn(async move {
            transport_b
                .call_tool("echo", json!({"label":"B"}), Some(tx_b))
                .await
        });

        let result_a = call_a.await.expect("join a").expect("result a");
        let result_b = call_b.await.expect("join b").expect("result b");
        server.abort();

        let update_a = rx_a.recv().await.expect("progress a");
        let update_b = rx_b.recv().await.expect("progress b");
        assert_eq!(result_a.content[0].as_text(), Some("A"));
        assert_eq!(result_b.content[0].as_text(), Some("B"));
        assert_eq!(update_a.message.as_deref(), Some("A"));
        assert_eq!(update_b.message.as_deref(), Some("B"));
        assert!(rx_a.try_recv().is_err());
        assert!(rx_b.try_recv().is_err());

        let captured = requests.lock().expect("requests lock");
        assert_eq!(
            captured
                .iter()
                .filter(|request| request["method"] == json!("initialize"))
                .count(),
            1
        );
        assert_eq!(
            captured
                .iter()
                .filter(|request| request["method"] == json!("tools/call"))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn http_server_without_progress_still_succeeds_with_progress_channel() {
        let (endpoint, server) = spawn_http_server(Arc::new(|request| {
            match request["method"].as_str().unwrap_or_default() {
                "initialize" => initialize_response(&request, json!({})),
                "tools/call" => tool_call_success_response(&request, "no-progress"),
                other => panic!("unexpected method: {other}"),
            }
        }))
        .await;
        let cfg = McpServerConnectionConfig::http("http_no_progress", endpoint);
        let transport = ProgressAwareHttpTransport::connect(&cfg).expect("connect transport");

        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let result = transport
            .call_tool("echo", json!({"message":"ok"}), Some(progress_tx))
            .await
            .expect("tool call");
        server.abort();

        assert_eq!(result.content[0].as_text(), Some("no-progress"));
        assert!(progress_rx.try_recv().is_err());
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

    // ---- handle_server_request tests ----

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
}
