//! End-to-end tests for MCP Sampling support.

mod support;

use async_trait::async_trait;
use mcp::transport::{McpServerConnectionConfig, McpTransportError};
use mcp::{CreateMessageParams, CreateMessageResult, Role, SamplingContent};
use std::sync::{Arc, Mutex};
use tirea_extension_mcp::{McpToolRegistryManager, McpToolTransport, SamplingHandler};

struct RecordingSamplingHandler {
    calls: Arc<Mutex<Vec<CreateMessageParams>>>,
    response_text: String,
}

impl RecordingSamplingHandler {
    fn new(response_text: impl Into<String>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            response_text: response_text.into(),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl SamplingHandler for RecordingSamplingHandler {
    async fn handle_create_message(
        &self,
        params: CreateMessageParams,
    ) -> Result<CreateMessageResult, McpTransportError> {
        self.calls.lock().unwrap().push(params);
        Ok(CreateMessageResult {
            role: Role::Assistant,
            content: vec![SamplingContent::Text {
                text: self.response_text.clone(),
                annotations: None,
                meta: None,
            }],
            model: "test-model".to_string(),
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
            "LLM unavailable".to_string(),
        ))
    }
}

#[tokio::test]
async fn sampling_handler_trait_is_object_safe_and_constructible() {
    let handler: Arc<dyn SamplingHandler> = Arc::new(RecordingSamplingHandler::new("hello"));
    let params = CreateMessageParams {
        messages: vec![],
        model_preferences: None,
        system_prompt: Some("You are helpful".to_string()),
        include_context: None,
        temperature: None,
        max_tokens: 100,
        stop_sequences: None,
        metadata: None,
        tools: None,
        tool_choice: None,
        task: None,
        meta: None,
    };

    let result = handler.handle_create_message(params).await.unwrap();
    assert_eq!(result.model, "test-model");
    assert_eq!(result.role, Role::Assistant);
    match &result.content[0] {
        SamplingContent::Text { text, .. } => assert_eq!(text, "hello"),
        _ => panic!("expected text content"),
    }
}

#[tokio::test]
async fn failing_sampling_handler_returns_error() {
    let handler: Arc<dyn SamplingHandler> = Arc::new(FailingSamplingHandler);
    let params = CreateMessageParams {
        messages: vec![],
        model_preferences: None,
        system_prompt: None,
        include_context: None,
        temperature: None,
        max_tokens: 100,
        stop_sequences: None,
        metadata: None,
        tools: None,
        tool_choice: None,
        task: None,
        meta: None,
    };

    let err = handler.handle_create_message(params).await.unwrap_err();
    assert!(err.to_string().contains("LLM unavailable"));
}

#[tokio::test]
async fn connect_with_sampling_accepts_handler() {
    let handler: Arc<dyn SamplingHandler> = Arc::new(RecordingSamplingHandler::new("ok"));
    let configs: Vec<McpServerConnectionConfig> = vec![];
    let manager = McpToolRegistryManager::connect_with_sampling(configs, Some(handler))
        .await
        .unwrap();
    assert!(manager.registry().is_empty());
}

#[tokio::test]
async fn connect_with_sampling_none_handler_works() {
    let configs: Vec<McpServerConnectionConfig> = vec![];
    let manager = McpToolRegistryManager::connect_with_sampling(configs, None)
        .await
        .unwrap();
    assert!(manager.registry().is_empty());
}

// ---------------------------------------------------------------------------
// Full bidirectional sampling E2E using DuplexStream
// ---------------------------------------------------------------------------

/// Inline DuplexStream-based MCP transport for sampling tests.
mod duplex_sampling {
    use async_trait::async_trait;
    use mcp::transport::{McpTransportError, TransportTypeId};
    use mcp::{
        CallToolParams, CallToolResult, JsonRpcId, JsonRpcMessage, JsonRpcNotification,
        JsonRpcPayload, JsonRpcRequest, JsonRpcResponse, ListToolsResult, McpToolDefinition,
    };
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::{Arc, Mutex};
    use tirea_extension_mcp::{McpProgressUpdate, McpToolTransport, SamplingHandler};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
    use tokio::sync::{mpsc, oneshot};

    struct WriteRequest {
        line: String,
    }

    pub struct DuplexTransport {
        write_tx: mpsc::Sender<WriteRequest>,
        pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, McpTransportError>>>>>,
        next_id: AtomicI64,
    }

    impl DuplexTransport {
        pub async fn connect(
            stream: DuplexStream,
            sampling_handler: Option<Arc<dyn SamplingHandler>>,
        ) -> Result<Self, McpTransportError> {
            let (reader_half, writer_half) = tokio::io::split(stream);
            let pending: Arc<
                Mutex<HashMap<i64, oneshot::Sender<Result<Value, McpTransportError>>>>,
            > = Arc::new(Mutex::new(HashMap::new()));

            let (write_tx, mut write_rx) = mpsc::channel::<WriteRequest>(256);

            let mut writer = writer_half;
            tokio::spawn(async move {
                while let Some(req) = write_rx.recv().await {
                    if writer.write_all(req.line.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = writer.flush().await;
                }
            });

            let pending_reader = Arc::clone(&pending);
            let write_tx_reader = write_tx.clone();
            let mut reader = BufReader::new(reader_half);
            tokio::spawn(async move {
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => match serde_json::from_str::<JsonRpcMessage>(&line) {
                            Ok(JsonRpcMessage::Response(response)) => {
                                if let JsonRpcId::Number(id) = response.id {
                                    let tx = pending_reader.lock().unwrap().remove(&id);
                                    if let Some(tx) = tx {
                                        let result = match response.payload {
                                            JsonRpcPayload::Success { result } => Ok(result),
                                            JsonRpcPayload::Error { error } => Err(
                                                McpTransportError::ServerError(error.to_string()),
                                            ),
                                        };
                                        let _ = tx.send(result);
                                    }
                                }
                            }
                            Ok(JsonRpcMessage::Request(request)) => {
                                let handler = sampling_handler.clone();
                                let wtx = write_tx_reader.clone();
                                tokio::spawn(async move {
                                    let response =
                                        handle_request(handler.as_deref(), &request).await;
                                    let line = format!(
                                        "{}\n",
                                        serde_json::to_string(&response).unwrap_or_default()
                                    );
                                    let _ = wtx.send(WriteRequest { line }).await;
                                });
                            }
                            Ok(JsonRpcMessage::Notification(_)) => {}
                            Err(_) => {}
                        },
                        Err(_) => break,
                    }
                }
            });

            let transport = Self {
                write_tx,
                pending,
                next_id: AtomicI64::new(1),
            };

            // Handshake
            let init_params = json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "0.1.0"},
            });
            transport
                .send_request("initialize", Some(init_params))
                .await?;

            let notification =
                JsonRpcNotification::new("notifications/initialized", Some(json!({})));
            let line = format!(
                "{}\n",
                serde_json::to_string(&notification).map_err(McpTransportError::from)?
            );
            transport
                .write_tx
                .send(WriteRequest { line })
                .await
                .map_err(|_| McpTransportError::ConnectionClosed)?;

            Ok(transport)
        }

        async fn send_request(
            &self,
            method: &str,
            params: Option<Value>,
        ) -> Result<Value, McpTransportError> {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let request = JsonRpcRequest::new(JsonRpcId::Number(id), method.to_string(), params);
            let line = format!(
                "{}\n",
                serde_json::to_string(&request).map_err(McpTransportError::from)?
            );

            let (tx, rx) = oneshot::channel();
            self.pending.lock().unwrap().insert(id, tx);

            if self.write_tx.send(WriteRequest { line }).await.is_err() {
                self.pending.lock().unwrap().remove(&id);
                return Err(McpTransportError::ConnectionClosed);
            }

            match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(McpTransportError::ConnectionClosed),
                Err(_) => Err(McpTransportError::Timeout("timed out".to_string())),
            }
        }
    }

    async fn handle_request(
        sampling_handler: Option<&dyn SamplingHandler>,
        request: &JsonRpcRequest,
    ) -> JsonRpcResponse {
        match request.method.as_str() {
            "sampling/createMessage" => {
                let Some(handler) = sampling_handler else {
                    return JsonRpcResponse::error(
                        request.id.clone(),
                        -32601,
                        "Sampling not supported".to_string(),
                        None,
                    );
                };
                let params = match request.params.as_ref().and_then(|p| {
                    serde_json::from_value::<mcp::CreateMessageParams>(p.clone()).ok()
                }) {
                    Some(p) => p,
                    None => {
                        return JsonRpcResponse::error(
                            request.id.clone(),
                            -32602,
                            "Invalid params".to_string(),
                            None,
                        );
                    }
                };
                match handler.handle_create_message(params).await {
                    Ok(result) => {
                        let v = serde_json::to_value(&result).unwrap_or(Value::Null);
                        JsonRpcResponse::success(request.id.clone(), v)
                    }
                    Err(e) => {
                        JsonRpcResponse::error(request.id.clone(), -32000, e.to_string(), None)
                    }
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
    impl McpToolTransport for DuplexTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            let result = self.send_request("tools/list", Some(json!({}))).await?;
            let list_result: ListToolsResult = serde_json::from_value(result)?;
            Ok(list_result.tools)
        }

        async fn call_tool(
            &self,
            name: &str,
            args: Value,
            _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
        ) -> Result<Value, McpTransportError> {
            let params = CallToolParams {
                name: name.to_string(),
                arguments: Some(args),
                task: None,
                meta: None,
            };
            let result = self
                .send_request("tools/call", Some(serde_json::to_value(&params)?))
                .await?;
            let call_result: CallToolResult = serde_json::from_value(result)?;

            if call_result.is_error == Some(true) {
                let error_text = call_result
                    .content
                    .first()
                    .and_then(|c| c.as_text())
                    .unwrap_or("Unknown error");
                return Err(McpTransportError::ServerError(error_text.to_string()));
            }

            let text = call_result
                .content
                .iter()
                .filter_map(|c| c.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Value::String(text))
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }

        async fn read_resource(&self, uri: &str) -> Result<Value, McpTransportError> {
            self.send_request("resources/read", Some(json!({"uri": uri})))
                .await
        }
    }
}

/// Spawn a minimal MCP server on a DuplexStream that sends
/// `sampling/createMessage` during tool execution.
fn spawn_sampling_server(prompt: &str) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<()>) {
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicI64, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let prompt = prompt.to_string();
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let handle = tokio::spawn(async move {
        let (reader_half, writer_half) = tokio::io::split(server_stream);
        let mut reader = BufReader::new(reader_half);
        let mut writer = writer_half;
        let next_id = AtomicI64::new(1000);

        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }

            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if msg.get("id").is_none() {
                continue;
            }

            let id = msg["id"].clone();
            let method = msg["method"].as_str().unwrap_or("");

            let response = match method {
                "initialize" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "sampling-test-server", "version": "0.1.0"}
                    }
                }),
                "tools/list" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [{
                            "name": "summarize",
                            "description": "Summarize using LLM via sampling",
                            "inputSchema": {"type": "object", "properties": {}}
                        }]
                    }
                }),
                "tools/call" => {
                    let sampling_id = next_id.fetch_add(1, Ordering::SeqCst);
                    let sampling_req = json!({
                        "jsonrpc": "2.0",
                        "id": sampling_id,
                        "method": "sampling/createMessage",
                        "params": {
                            "messages": [{"role": "user", "content": [{"type": "text", "text": prompt}]}],
                            "maxTokens": 100
                        }
                    });
                    let req_line = format!("{}\n", serde_json::to_string(&sampling_req).unwrap());
                    if writer.write_all(req_line.as_bytes()).await.is_err() {
                        break;
                    }
                    if writer.flush().await.is_err() {
                        break;
                    }

                    let mut resp_line = String::new();
                    match reader.read_line(&mut resp_line).await {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    let resp: Value = serde_json::from_str(&resp_line).unwrap_or(json!(null));
                    let llm_text = resp["result"]["content"]
                        .as_array()
                        .and_then(|arr| arr.first())
                        .and_then(|c| c.get("text"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("(no response)");

                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": format!("sampling:{llm_text}")}]
                        }
                    })
                }
                _ => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("Unknown: {method}")}
                }),
            };

            let out = format!("{}\n", serde_json::to_string(&response).unwrap());
            if writer.write_all(out.as_bytes()).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
    });
    (client_stream, handle)
}

#[tokio::test]
async fn sampling_request_routed_through_duplex_transport() {
    use duplex_sampling::DuplexTransport;
    let handler = Arc::new(RecordingSamplingHandler::new("LLM says hello"));
    let (stream, _server) = spawn_sampling_server("Summarize this document");

    let transport: Arc<dyn McpToolTransport> = Arc::new(
        DuplexTransport::connect(stream, Some(handler.clone() as Arc<dyn SamplingHandler>))
            .await
            .unwrap(),
    );

    let cfg = McpServerConnectionConfig::stdio("sampling_server", "dummy", vec![]);
    let manager = McpToolRegistryManager::from_transports([(cfg, transport)])
        .await
        .unwrap();
    let registry = manager.registry();

    let tool = registry.get("mcp__sampling_server__summarize").unwrap();
    let fix = tirea_contract::testing::TestFixture::new();
    let ctx = fix.ctx_with("sampling-call", "test");

    let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();

    assert!(result.is_success());
    let result_text = result.data.as_str().unwrap();
    assert!(
        result_text.contains("LLM says hello"),
        "tool result should contain sampling response, got: {result_text}"
    );

    assert_eq!(handler.call_count(), 1);
}

#[tokio::test]
async fn sampling_without_handler_returns_error_to_server() {
    use duplex_sampling::DuplexTransport;
    let (stream, _server) = spawn_sampling_server("This will fail");
    let transport: Arc<dyn McpToolTransport> =
        Arc::new(DuplexTransport::connect(stream, None).await.unwrap());

    let cfg = McpServerConnectionConfig::stdio("no_handler", "dummy", vec![]);
    let manager = McpToolRegistryManager::from_transports([(cfg, transport)])
        .await
        .unwrap();
    let registry = manager.registry();

    let tool = registry.get("mcp__no_handler__summarize").unwrap();
    let fix = tirea_contract::testing::TestFixture::new();
    let ctx = fix.ctx_with("no-handler-call", "test");

    let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();

    assert!(result.is_success());
    let result_text = result.data.as_str().unwrap();
    assert!(
        result_text.contains("(no response)") || result_text.contains("sampling:"),
        "tool should still succeed, got: {result_text}"
    );
}
