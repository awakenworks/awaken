//! Integration tests for the awaken-ext-mcp crate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use awaken_contract::{AgentSpec, CancellationToken, contract::tool::ToolCallContext};
use awaken_ext_mcp::{
    McpError, McpProgressUpdate, McpPromptArgument, McpPromptDefinition, McpPromptMessage,
    McpPromptResult, McpResourceDefinition, McpServerConnectionConfig, McpToolRegistryManager,
    McpToolTransport, SamplingHandler, SamplingHandlerFactory,
};
use mcp::transport::{McpTransportError, ServerCapabilities, TransportTypeId};
use mcp::{
    CallToolResult, CreateMessageParams, CreateMessageResult, McpToolDefinition, ToolContent,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

// ── Test helpers ──

fn ok_text_result(text: &str) -> CallToolResult {
    CallToolResult {
        content: vec![ToolContent::text(text)],
        structured_content: None,
        is_error: None,
    }
}

fn cfg(name: &str) -> McpServerConnectionConfig {
    McpServerConnectionConfig::stdio(name, "node", vec!["server.js".to_string()])
}

#[derive(Debug, Clone)]
struct FakeTransport {
    tools: Arc<Mutex<Vec<McpToolDefinition>>>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    fail_next_list: Arc<Mutex<Option<String>>>,
    list_calls: Arc<AtomicUsize>,
}

impl FakeTransport {
    fn new(tools: Vec<McpToolDefinition>) -> Self {
        Self {
            tools: Arc::new(Mutex::new(tools)),
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_next_list: Arc::new(Mutex::new(None)),
            list_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn set_tools(&self, tools: Vec<McpToolDefinition>) {
        *self.tools.lock().unwrap() = tools;
    }

    fn fail_next_list(&self, message: impl Into<String>) {
        *self.fail_next_list.lock().unwrap() = Some(message.into());
    }

    fn list_calls(&self) -> usize {
        self.list_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl McpToolTransport for FakeTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(message) = self.fail_next_list.lock().unwrap().take() {
            return Err(McpTransportError::TransportError(message));
        }
        Ok(self.tools.lock().unwrap().clone())
    }

    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        _progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
        _context: awaken_ext_mcp::McpCallContext,
    ) -> Result<CallToolResult, McpTransportError> {
        self.calls.lock().unwrap().push((name.to_string(), args));
        Ok(ok_text_result("ok"))
    }

    fn transport_type(&self) -> TransportTypeId {
        TransportTypeId::Stdio
    }
}

#[derive(Debug, Clone)]
struct FakeProgressTransport;

#[async_trait]
impl McpToolTransport for FakeProgressTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        Ok(vec![McpToolDefinition::new("echo")])
    }

    async fn call_tool(
        &self,
        _name: &str,
        _args: Value,
        progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
        _context: awaken_ext_mcp::McpCallContext,
    ) -> Result<CallToolResult, McpTransportError> {
        if let Some(progress_tx) = progress_tx {
            let _ = progress_tx.try_send(McpProgressUpdate {
                progress: 3.0,
                total: Some(10.0),
                message: Some("working".to_string()),
            });
            let _ = progress_tx.try_send(McpProgressUpdate {
                progress: 10.0,
                total: Some(10.0),
                message: Some("done".to_string()),
            });
        }
        Ok(ok_text_result("ok"))
    }

    fn transport_type(&self) -> TransportTypeId {
        TransportTypeId::Stdio
    }
}

#[derive(Debug, Clone)]
struct FakeProgressFloodTransport {
    attempted: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
}

#[async_trait]
impl McpToolTransport for FakeProgressFloodTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        Ok(vec![McpToolDefinition::new("echo")])
    }

    async fn call_tool(
        &self,
        _name: &str,
        _args: Value,
        progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
        _context: awaken_ext_mcp::McpCallContext,
    ) -> Result<CallToolResult, McpTransportError> {
        if let Some(progress_tx) = progress_tx {
            for i in 0..10_000usize {
                self.attempted.fetch_add(1, Ordering::SeqCst);
                if progress_tx
                    .try_send(McpProgressUpdate {
                        progress: i as f64,
                        total: Some(10_000.0),
                        message: None,
                    })
                    .is_ok()
                {
                    self.accepted.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        Ok(ok_text_result("ok"))
    }

    fn transport_type(&self) -> TransportTypeId {
        TransportTypeId::Stdio
    }
}

#[derive(Debug, Clone)]
struct FakeStructuredTransport {
    result: CallToolResult,
}

#[async_trait]
impl McpToolTransport for FakeStructuredTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        Ok(vec![McpToolDefinition::new("echo")])
    }

    async fn call_tool(
        &self,
        _name: &str,
        _args: Value,
        _progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
        _context: awaken_ext_mcp::McpCallContext,
    ) -> Result<CallToolResult, McpTransportError> {
        Ok(self.result.clone())
    }

    fn transport_type(&self) -> TransportTypeId {
        TransportTypeId::Stdio
    }
}

type PromptRequestLog = Arc<Mutex<Vec<(String, Option<HashMap<String, String>>)>>>;

#[derive(Debug, Clone)]
struct FakeCatalogTransport {
    prompts: Vec<McpPromptDefinition>,
    resources: Vec<McpResourceDefinition>,
    prompt_result: McpPromptResult,
    read_resource_result: Value,
    prompt_requests: PromptRequestLog,
    resource_requests: Arc<Mutex<Vec<String>>>,
    prompt_list_calls: Arc<AtomicUsize>,
    resource_list_calls: Arc<AtomicUsize>,
    capabilities: Option<ServerCapabilities>,
}

impl FakeCatalogTransport {
    fn new(
        prompts: Vec<McpPromptDefinition>,
        resources: Vec<McpResourceDefinition>,
        prompt_result: McpPromptResult,
        read_resource_result: Value,
    ) -> Self {
        Self {
            prompts,
            resources,
            prompt_result,
            read_resource_result,
            prompt_requests: Arc::new(Mutex::new(Vec::new())),
            resource_requests: Arc::new(Mutex::new(Vec::new())),
            prompt_list_calls: Arc::new(AtomicUsize::new(0)),
            resource_list_calls: Arc::new(AtomicUsize::new(0)),
            capabilities: None,
        }
    }

    fn with_capabilities(mut self, capabilities: ServerCapabilities) -> Self {
        self.capabilities = Some(capabilities);
        self
    }

    fn prompt_requests(&self) -> Vec<(String, Option<HashMap<String, String>>)> {
        self.prompt_requests.lock().unwrap().clone()
    }

    fn resource_requests(&self) -> Vec<String> {
        self.resource_requests.lock().unwrap().clone()
    }

    fn prompt_list_calls(&self) -> usize {
        self.prompt_list_calls.load(Ordering::SeqCst)
    }

    fn resource_list_calls(&self) -> usize {
        self.resource_list_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl McpToolTransport for FakeCatalogTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        Ok(Vec::new())
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
        self.prompt_list_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.prompts.clone())
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<McpPromptResult, McpTransportError> {
        self.prompt_requests
            .lock()
            .unwrap()
            .push((name.to_string(), arguments));
        Ok(self.prompt_result.clone())
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
        self.resource_list_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.resources.clone())
    }

    async fn call_tool(
        &self,
        _name: &str,
        _args: Value,
        _progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
        _context: awaken_ext_mcp::McpCallContext,
    ) -> Result<CallToolResult, McpTransportError> {
        Ok(ok_text_result("ok"))
    }

    fn transport_type(&self) -> TransportTypeId {
        TransportTypeId::Stdio
    }

    async fn server_capabilities(&self) -> Result<Option<ServerCapabilities>, McpTransportError> {
        Ok(self.capabilities.clone())
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, McpTransportError> {
        self.resource_requests.lock().unwrap().push(uri.to_string());
        Ok(self.read_resource_result.clone())
    }
}

// ── UI transport ──

#[derive(Debug, Clone)]
struct FakeUiTransport {
    tools: Vec<McpToolDefinition>,
    resources: HashMap<String, (String, String)>, // uri -> (text, mimeType)
    read_delay: Option<Duration>,
}

impl FakeUiTransport {
    fn new(tools: Vec<McpToolDefinition>) -> Self {
        Self {
            tools,
            resources: HashMap::new(),
            read_delay: None,
        }
    }

    fn with_resource(
        mut self,
        uri: impl Into<String>,
        text: impl Into<String>,
        mime: impl Into<String>,
    ) -> Self {
        self.resources
            .insert(uri.into(), (text.into(), mime.into()));
        self
    }

    fn with_read_delay(mut self, delay: Duration) -> Self {
        self.read_delay = Some(delay);
        self
    }
}

#[async_trait]
impl McpToolTransport for FakeUiTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        Ok(self.tools.clone())
    }

    async fn call_tool(
        &self,
        _name: &str,
        _args: Value,
        _progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
        _context: awaken_ext_mcp::McpCallContext,
    ) -> Result<CallToolResult, McpTransportError> {
        Ok(ok_text_result("ok"))
    }

    fn transport_type(&self) -> TransportTypeId {
        TransportTypeId::Stdio
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, McpTransportError> {
        if let Some(delay) = self.read_delay {
            tokio::time::sleep(delay).await;
        }
        match self.resources.get(uri) {
            Some((text, mime)) => Ok(json!({
                "contents": [{"uri": uri, "text": text, "mimeType": mime}]
            })),
            None => Err(McpTransportError::ServerError(format!(
                "not found: {}",
                uri
            ))),
        }
    }
}

// ── HTTP test helpers ──

#[derive(Clone)]
struct HttpRequestSpec {
    /// HTTP method (e.g. `GET`, `POST`, `DELETE`). Captured from the
    /// first line of the request so listening-stream tests can
    /// distinguish the POST initialize/tools-call traffic from the
    /// GET that opens the server-push SSE stream.
    method: String,
    headers: std::collections::HashMap<String, String>,
    /// Parsed JSON body. `Value::Null` when the request had no body
    /// (GET, OPTIONS, or any other Content-Length: 0 request) — the
    /// previous parser failed silently in that case.
    body: Value,
}

#[derive(Clone)]
struct HttpResponseSpec {
    status: u16,
    content_type: &'static str,
    body: String,
    headers: Vec<(String, String)>,
}

impl HttpResponseSpec {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.to_string(),
            headers: Vec::new(),
        }
    }

    fn json_with_headers(
        body: Value,
        headers: Vec<(impl Into<String>, impl Into<String>)>,
    ) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.to_string(),
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/plain",
            body: body.into(),
            headers: Vec::new(),
        }
    }

    fn accepted() -> Self {
        Self {
            status: 202,
            content_type: "text/plain",
            body: String::new(),
            headers: Vec::new(),
        }
    }

    fn sse(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body: body.into(),
            headers: Vec::new(),
        }
    }

    fn sse_with_headers(
        body: impl Into<String>,
        headers: Vec<(impl Into<String>, impl Into<String>)>,
    ) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body: body.into(),
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
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

async fn write_http_response(stream: &mut TcpStream, response: HttpResponseSpec) {
    let payload = response.body;
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        status_text(response.status),
        response.content_type,
        payload.len()
    );
    for (key, value) in response.headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(payload.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn parse_headers(raw: &str) -> std::collections::HashMap<String, String> {
    raw.lines()
        .skip(1)
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect()
}

async fn read_http_request(stream: &mut TcpStream) -> Option<HttpRequestSpec> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 1024];
    let (header_end_pos, body_len) = loop {
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
        .unwrap_or("")
        .to_string();
    // Bodies with Content-Length: 0 (GET, OPTIONS, body-less DELETE)
    // are common in spec-compliant clients — surface them as
    // `Value::Null` instead of failing parse, so handlers can switch
    // on `method`.
    let body = if body_len == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&buf[header_end_pos..header_end_pos + body_len]).ok()?
    };
    Some(HttpRequestSpec {
        method,
        headers: parse_headers(headers_text),
        body,
    })
}

async fn spawn_http_server(
    handler: Arc<dyn Fn(HttpRequestSpec) -> HttpResponseSpec + Send + Sync>,
) -> (String, tokio::task::JoinHandle<()>) {
    spawn_http_server_with_response_observer(handler, None).await
}

async fn spawn_http_server_with_response_observer(
    handler: Arc<dyn Fn(HttpRequestSpec) -> HttpResponseSpec + Send + Sync>,
    response_observer: Option<Arc<dyn Fn(HttpRequestSpec) + Send + Sync>>,
) -> (String, tokio::task::JoinHandle<()>) {
    let response_handler = response_observer.map(|observer| {
        Arc::new(move |request| {
            observer(request);
            HttpResponseSpec::accepted()
        }) as Arc<dyn Fn(HttpRequestSpec) -> HttpResponseSpec + Send + Sync>
    });
    spawn_http_server_with_response_handler(handler, response_handler).await
}

async fn spawn_http_server_with_response_handler(
    handler: Arc<dyn Fn(HttpRequestSpec) -> HttpResponseSpec + Send + Sync>,
    response_handler: Option<Arc<dyn Fn(HttpRequestSpec) -> HttpResponseSpec + Send + Sync>>,
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
            let response_handler = response_handler.clone();
            tokio::spawn(async move {
                let Some(request) = read_http_request(&mut stream).await else {
                    return;
                };
                let response = if request.body["method"].is_null()
                    && (request.body.get("result").is_some() || request.body.get("error").is_some())
                {
                    if let Some(response_handler) = response_handler {
                        response_handler(request.clone())
                    } else {
                        HttpResponseSpec::accepted()
                    }
                } else {
                    handler(request)
                };
                write_http_response(&mut stream, response).await;
            });
        }
    });
    (format!("http://{}", addr), handle)
}

fn initialize_response(request: &HttpRequestSpec, capabilities: Value) -> HttpResponseSpec {
    HttpResponseSpec::json_with_headers(
        json!({
            "jsonrpc": "2.0",
            "id": request.body["id"].clone(),
            "result": {
                "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                "capabilities": capabilities,
                "serverInfo": {
                    "name": "test-server",
                    "version": "1.0.0"
                }
            }
        }),
        vec![("MCP-Session-Id", "test-session")],
    )
}

async fn wait_until(
    timeout: Duration,
    step: Duration,
    mut predicate: impl FnMut() -> bool,
) -> bool {
    let start = Instant::now();
    while start.elapsed() <= timeout {
        if predicate() {
            return true;
        }
        tokio::time::sleep(step).await;
    }
    predicate()
}

// ── Tests ──

#[tokio::test]
async fn registry_discovers_tools_and_executes_calls() {
    let fake = Arc::new(FakeTransport::new(vec![
        McpToolDefinition::new("echo").with_title("Echo"),
    ]));
    let transport = fake.clone() as Arc<dyn McpToolTransport>;

    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport.clone())])
        .await
        .unwrap();
    let reg = manager.registry();

    let id = reg.ids().into_iter().find(|x| x.contains("echo")).unwrap();
    let tool = reg.get(&id).unwrap();

    let desc = tool.descriptor();
    assert_eq!(desc.id, id);
    assert_eq!(desc.name, "Echo");
    assert_eq!(
        desc.metadata.get("mcp.server").and_then(|v| v.as_str()),
        Some("s1")
    );
    assert_eq!(
        desc.metadata.get("mcp.tool").and_then(|v| v.as_str()),
        Some("echo")
    );

    let ctx = ToolCallContext::test_default();
    let res = tool
        .execute(serde_json::json!({"a": 1}), &ctx)
        .await
        .unwrap();
    assert!(res.result.is_success());

    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "echo");
}

#[tokio::test]
async fn registry_refresh_discovers_new_tools_without_rebuild() {
    let fake = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]));
    let transport = fake.clone() as Arc<dyn McpToolTransport>;

    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport.clone())])
        .await
        .unwrap();
    let reg = manager.registry();
    assert_eq!(manager.version(), 1);

    fake.set_tools(vec![
        McpToolDefinition::new("echo"),
        McpToolDefinition::new("sum"),
    ]);

    let version = manager.refresh().await.unwrap();
    assert_eq!(version, 2);
    assert!(reg.ids().into_iter().any(|id| id.contains("sum")));
}

#[tokio::test]
async fn failed_refresh_keeps_last_good_snapshot() {
    let fake = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]));
    let transport = fake.clone() as Arc<dyn McpToolTransport>;

    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport.clone())])
        .await
        .unwrap();
    let reg = manager.registry();
    let initial_ids = reg.ids();
    let initial_version = manager.version();

    fake.fail_next_list("temporary outage");

    let version = manager.refresh().await.unwrap();
    assert_eq!(version, initial_version.saturating_add(1));
    assert_eq!(manager.version(), version);
    assert_eq!(reg.ids(), initial_ids);
    let health = manager.server_health("s1").unwrap();
    assert_eq!(
        health.last_error.as_deref(),
        Some("mcp transport error: Transport error: temporary outage")
    );
    assert_eq!(health.consecutive_failures, 1);
    assert!(health.last_attempt_at.is_some());
    assert!(health.last_success_at.is_some());
}

#[tokio::test]
async fn refresh_health_clears_error_after_recovery() {
    let fake = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]));
    let transport = fake.clone() as Arc<dyn McpToolTransport>;

    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();

    fake.fail_next_list("temporary outage");
    let _ = manager
        .refresh()
        .await
        .expect("refresh should keep last good snapshot");

    let failed_health = manager.server_health("s1").unwrap();
    assert_eq!(failed_health.consecutive_failures, 1);
    assert!(failed_health.last_error.is_some());

    let _ = manager.refresh().await.expect("refresh should recover");
    let recovered_health = manager.server_health("s1").unwrap();
    assert_eq!(recovered_health.consecutive_failures, 0);
    assert!(recovered_health.last_error.is_none());
    assert!(recovered_health.last_success_at.is_some());
}

#[tokio::test]
async fn periodic_refresh_updates_snapshot_and_can_stop() {
    let fake = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]));
    let transport = fake.clone() as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();

    manager
        .start_periodic_refresh(Duration::from_millis(20))
        .expect("start periodic refresh");
    assert!(manager.periodic_refresh_running());

    fake.set_tools(vec![
        McpToolDefinition::new("echo"),
        McpToolDefinition::new("sum"),
    ]);

    let observed = wait_until(
        Duration::from_millis(400),
        Duration::from_millis(20),
        || manager.version() >= 2 && reg.ids().iter().any(|id| id.contains("sum")),
    )
    .await;
    assert!(observed, "periodic refresh should publish updated tools");
    assert!(
        fake.list_calls() >= 2,
        "list_tools should be called periodically"
    );

    assert!(manager.stop_periodic_refresh().await);
    assert!(!manager.periodic_refresh_running());

    let version_after_stop = manager.version();
    fake.set_tools(vec![
        McpToolDefinition::new("echo"),
        McpToolDefinition::new("sum"),
        McpToolDefinition::new("mul"),
    ]);
    tokio::time::sleep(Duration::from_millis(80)).await;

    assert_eq!(
        manager.version(),
        version_after_stop,
        "version should not change after periodic refresh stops"
    );
    assert!(
        !reg.ids().iter().any(|id| id.contains("mul")),
        "stopped periodic refresh should not publish new tools"
    );
}

#[tokio::test]
async fn periodic_refresh_rejects_duplicate_start() {
    let fake = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]));
    let transport = fake.clone() as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();

    manager
        .start_periodic_refresh(Duration::from_millis(100))
        .expect("start periodic refresh");
    let err = manager
        .start_periodic_refresh(Duration::from_millis(100))
        .err()
        .unwrap();
    assert!(matches!(err, McpError::PeriodicRefreshAlreadyRunning));
    assert!(manager.stop_periodic_refresh().await);
}

#[tokio::test]
async fn periodic_refresh_rejects_zero_interval() {
    let fake = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]));
    let transport = fake.clone() as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();

    let err = manager
        .start_periodic_refresh(Duration::from_millis(0))
        .err()
        .unwrap();
    assert!(matches!(err, McpError::InvalidRefreshInterval));
}

#[tokio::test]
async fn sanitize_rejects_empty_component() {
    let err = awaken_ext_mcp::id_mapping::to_tool_id("   ", "echo")
        .err()
        .unwrap();
    assert!(matches!(err, McpError::InvalidToolIdComponent(_)));
}

#[tokio::test]
async fn tool_id_conflict_is_an_error() {
    let transport = Arc::new(FakeTransport::new(vec![
        McpToolDefinition::new("a-b"),
        McpToolDefinition::new("a_b"),
    ])) as Arc<dyn McpToolTransport>;

    let err = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .err()
        .unwrap();
    assert!(matches!(err, McpError::ToolIdConflict(_)));
}

#[tokio::test]
async fn mcp_tool_forwards_progress_to_activity_reports() {
    let transport = Arc::new(FakeProgressTransport) as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();
    let tool_id = reg
        .ids()
        .into_iter()
        .find(|id| id.contains("echo"))
        .unwrap();
    let tool = reg.get(&tool_id).unwrap();

    let ctx = ToolCallContext::test_default();
    let result = tool.execute(json!({}), &ctx).await.unwrap();
    assert!(result.result.is_success());
    // Progress events are reported via activity_sink; with test_default (no sink),
    // this just verifies the progress handling doesn't cause errors.
}

#[tokio::test]
async fn mcp_tool_progress_flood_is_bounded_and_non_fatal() {
    let attempted = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::new(AtomicUsize::new(0));
    let transport = Arc::new(FakeProgressFloodTransport {
        attempted: Arc::clone(&attempted),
        accepted: Arc::clone(&accepted),
    }) as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();
    let tool = reg.get("mcp__s1__echo").unwrap();

    let ctx = ToolCallContext::test_default();
    let result = tool.execute(json!({}), &ctx).await.unwrap();

    assert!(result.result.is_success());
    assert_eq!(attempted.load(Ordering::SeqCst), 10_000);
    assert!(
        accepted.load(Ordering::SeqCst) < attempted.load(Ordering::SeqCst),
        "progress sender should apply bounded backpressure/drop instead of buffering flood"
    );
}

#[tokio::test]
async fn structured_mcp_results_are_preserved_in_tool_output() {
    let transport = Arc::new(FakeStructuredTransport {
        result: CallToolResult {
            content: vec![ToolContent::Resource {
                uri: "file://report.json".to_string(),
                mime_type: Some("application/json".to_string()),
            }],
            structured_content: Some(json!({"sum": 3, "values": [1, 2]})),
            is_error: None,
        },
    }) as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.contains("echo"))
        .expect("echo tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let ctx = ToolCallContext::test_default();
    let result = tool.execute(json!({}), &ctx).await.expect("tool result");
    assert!(result.result.is_success());

    // Structured content should be preserved in result.metadata
    assert_eq!(
        result.result.metadata["mcp.result.structuredContent"]["sum"],
        json!(3)
    );
    assert!(result.result.metadata["mcp.result.content"].is_array());
}

#[tokio::test]
async fn manager_lists_prompts_and_resources_across_servers() {
    let transport_a = Arc::new(FakeCatalogTransport::new(
        vec![McpPromptDefinition {
            name: "review".to_string(),
            title: Some("Review".to_string()),
            description: Some("Review code".to_string()),
            arguments: vec![McpPromptArgument {
                name: "path".to_string(),
                description: Some("Target path".to_string()),
                required: true,
            }],
        }],
        vec![McpResourceDefinition {
            uri: "file://alpha.md".to_string(),
            name: "alpha".to_string(),
            title: Some("Alpha".to_string()),
            description: Some("Alpha doc".to_string()),
            mime_type: Some("text/markdown".to_string()),
            size: Some(12),
        }],
        McpPromptResult {
            description: Some("unused".to_string()),
            messages: Vec::new(),
        },
        json!({}),
    )) as Arc<dyn McpToolTransport>;
    let transport_b = Arc::new(FakeCatalogTransport::new(
        vec![McpPromptDefinition {
            name: "fix".to_string(),
            title: Some("Fix".to_string()),
            description: Some("Fix issue".to_string()),
            arguments: Vec::new(),
        }],
        vec![McpResourceDefinition {
            uri: "file://beta.md".to_string(),
            name: "beta".to_string(),
            title: Some("Beta".to_string()),
            description: Some("Beta doc".to_string()),
            mime_type: Some("text/markdown".to_string()),
            size: Some(8),
        }],
        McpPromptResult {
            description: Some("unused".to_string()),
            messages: Vec::new(),
        },
        json!({}),
    )) as Arc<dyn McpToolTransport>;

    let manager = McpToolRegistryManager::from_transports([
        (cfg("s2"), transport_b),
        (cfg("s1"), transport_a),
    ])
    .await
    .unwrap();

    let prompts = manager.list_prompts().await.unwrap();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].server_name, "s1");
    assert_eq!(prompts[0].prompt.name, "review");
    assert_eq!(prompts[1].server_name, "s2");
    assert_eq!(prompts[1].prompt.name, "fix");

    let resources = manager.list_resources().await.unwrap();
    assert_eq!(resources.len(), 2);
    assert_eq!(resources[0].server_name, "s1");
    assert_eq!(resources[0].resource.uri, "file://alpha.md");
    assert_eq!(resources[1].server_name, "s2");
    assert_eq!(resources[1].resource.uri, "file://beta.md");
}

#[tokio::test]
async fn manager_skips_prompt_and_resource_listing_for_servers_without_capabilities() {
    let unsupported = Arc::new(
        FakeCatalogTransport::new(
            vec![McpPromptDefinition {
                name: "hidden".to_string(),
                title: None,
                description: None,
                arguments: Vec::new(),
            }],
            vec![McpResourceDefinition {
                uri: "file://hidden.md".to_string(),
                name: "hidden".to_string(),
                title: None,
                description: None,
                mime_type: None,
                size: None,
            }],
            McpPromptResult {
                description: None,
                messages: Vec::new(),
            },
            json!({}),
        )
        .with_capabilities(ServerCapabilities {
            prompts: None,
            resources: None,
            ..ServerCapabilities::default()
        }),
    );
    let supported = Arc::new(
        FakeCatalogTransport::new(
            vec![McpPromptDefinition {
                name: "review".to_string(),
                title: None,
                description: Some("Review".to_string()),
                arguments: Vec::new(),
            }],
            vec![McpResourceDefinition {
                uri: "file://guide.md".to_string(),
                name: "guide".to_string(),
                title: None,
                description: Some("Guide".to_string()),
                mime_type: Some("text/markdown".to_string()),
                size: None,
            }],
            McpPromptResult {
                description: None,
                messages: Vec::new(),
            },
            json!({}),
        )
        .with_capabilities(ServerCapabilities {
            prompts: Some(mcp::transport::PromptsCapabilities::default()),
            resources: Some(mcp::transport::ResourcesCapabilities::default()),
            ..ServerCapabilities::default()
        }),
    );

    let manager = McpToolRegistryManager::from_transports([
        (
            cfg("unsupported"),
            unsupported.clone() as Arc<dyn McpToolTransport>,
        ),
        (
            cfg("supported"),
            supported.clone() as Arc<dyn McpToolTransport>,
        ),
    ])
    .await
    .unwrap();

    let prompts = manager.list_prompts().await.unwrap();
    let resources = manager.list_resources().await.unwrap();

    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].server_name, "supported");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].server_name, "supported");
    assert_eq!(unsupported.prompt_list_calls(), 0);
    assert_eq!(unsupported.resource_list_calls(), 0);
    assert_eq!(supported.prompt_list_calls(), 1);
    assert_eq!(supported.resource_list_calls(), 1);
}

#[tokio::test]
async fn manager_keeps_unsupported_fallback_when_capabilities_are_unknown() {
    #[derive(Debug, Clone)]
    struct UnsupportedCatalogTransport;

    #[async_trait]
    impl McpToolTransport for UnsupportedCatalogTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(Vec::new())
        }

        async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
            Err(McpTransportError::TransportError(
                "list_prompts not supported".to_string(),
            ))
        }

        async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
            Err(McpTransportError::TransportError(
                "list_resources not supported".to_string(),
            ))
        }

        async fn call_tool(
            &self,
            _name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
            _context: awaken_ext_mcp::McpCallContext,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(ok_text_result("ok"))
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }
    }

    let manager = McpToolRegistryManager::from_transports([(
        cfg("unknown"),
        Arc::new(UnsupportedCatalogTransport) as Arc<dyn McpToolTransport>,
    )])
    .await
    .unwrap();

    assert!(manager.list_prompts().await.unwrap().is_empty());
    assert!(manager.list_resources().await.unwrap().is_empty());
}

#[tokio::test]
async fn manager_get_prompt_and_read_resource_fail_fast_when_capability_missing() {
    let transport = Arc::new(
        FakeCatalogTransport::new(
            vec![McpPromptDefinition {
                name: "review".to_string(),
                title: None,
                description: None,
                arguments: Vec::new(),
            }],
            vec![McpResourceDefinition {
                uri: "file://guide.md".to_string(),
                name: "guide".to_string(),
                title: None,
                description: None,
                mime_type: None,
                size: None,
            }],
            McpPromptResult {
                description: None,
                messages: Vec::new(),
            },
            json!({"contents": []}),
        )
        .with_capabilities(ServerCapabilities {
            prompts: None,
            resources: None,
            ..ServerCapabilities::default()
        }),
    );
    let manager = McpToolRegistryManager::from_transports([(
        cfg("s1"),
        transport.clone() as Arc<dyn McpToolTransport>,
    )])
    .await
    .unwrap();

    let prompt_err = manager
        .get_prompt("s1", "review", None)
        .await
        .expect_err("unsupported prompt capability should fail");
    let resource_err = manager
        .read_resource("s1", "file://guide.md")
        .await
        .expect_err("unsupported resource capability should fail");

    assert!(matches!(
        prompt_err,
        McpError::UnsupportedCapability {
            server_name,
            capability
        } if server_name == "s1" && capability == "prompts"
    ));
    assert!(matches!(
        resource_err,
        McpError::UnsupportedCapability {
            server_name,
            capability
        } if server_name == "s1" && capability == "resources"
    ));
    assert!(transport.prompt_requests().is_empty());
    assert!(transport.resource_requests().is_empty());
}

#[tokio::test]
async fn manager_get_prompt_and_read_resource_route_to_selected_server() {
    let transport = Arc::new(FakeCatalogTransport::new(
        vec![McpPromptDefinition {
            name: "review".to_string(),
            title: Some("Review".to_string()),
            description: Some("Review code".to_string()),
            arguments: vec![McpPromptArgument {
                name: "path".to_string(),
                description: None,
                required: true,
            }],
        }],
        vec![McpResourceDefinition {
            uri: "file://alpha.md".to_string(),
            name: "alpha".to_string(),
            title: None,
            description: None,
            mime_type: Some("text/markdown".to_string()),
            size: None,
        }],
        McpPromptResult {
            description: Some("Review prompt".to_string()),
            messages: vec![McpPromptMessage {
                role: "user".to_string(),
                content: json!({"type": "text", "text": "Review src/lib.rs"}),
            }],
        },
        json!({
            "contents": [{
                "uri": "file://alpha.md",
                "text": "# Alpha",
                "mimeType": "text/markdown"
            }]
        }),
    ));
    let manager = McpToolRegistryManager::from_transports([(
        cfg("s1"),
        transport.clone() as Arc<dyn McpToolTransport>,
    )])
    .await
    .unwrap();

    let prompt = manager
        .get_prompt(
            "s1",
            "review",
            Some(HashMap::from([(
                "path".to_string(),
                "src/lib.rs".to_string(),
            )])),
        )
        .await
        .unwrap();
    assert_eq!(prompt.description.as_deref(), Some("Review prompt"));
    assert_eq!(prompt.messages.len(), 1);
    assert_eq!(prompt.messages[0].role, "user");

    let prompt_requests = transport.prompt_requests();
    assert_eq!(prompt_requests.len(), 1);
    assert_eq!(prompt_requests[0].0, "review");
    assert_eq!(
        prompt_requests[0]
            .1
            .as_ref()
            .and_then(|args| args.get("path")),
        Some(&"src/lib.rs".to_string())
    );

    let resource = manager
        .read_resource("s1", "file://alpha.md")
        .await
        .unwrap();
    assert_eq!(resource["contents"][0]["text"], json!("# Alpha"));
}

#[tokio::test]
async fn manager_prompt_and_resource_apis_reject_unknown_server() {
    let manager = McpToolRegistryManager::from_transports([(
        cfg("s1"),
        Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]))
            as Arc<dyn McpToolTransport>,
    )])
    .await
    .unwrap();

    let prompt_err = manager
        .get_prompt("missing", "review", None)
        .await
        .expect_err("unknown server should fail");
    assert!(matches!(
        prompt_err,
        McpError::UnknownServer(name) if name == "missing"
    ));

    let resource_err = manager
        .read_resource("missing", "file://alpha.md")
        .await
        .expect_err("unknown server should fail");
    assert!(matches!(
        resource_err,
        McpError::UnknownServer(name) if name == "missing"
    ));
}

// ── UI metadata tests ──

#[tokio::test]
async fn mcp_tool_execute_fetches_ui_resource() {
    let mut def = McpToolDefinition::new("chart");
    def.meta = Some(json!({"ui": {"resourceUri": "ui://chart/render"}}));

    let transport = Arc::new(FakeUiTransport::new(vec![def.clone()]).with_resource(
        "ui://chart/render",
        "<html>chart</html>",
        "text/html",
    ));

    let manager = McpToolRegistryManager::from_transports([(
        cfg("s1"),
        transport as Arc<dyn McpToolTransport>,
    )])
    .await
    .unwrap();
    let reg = manager.registry();
    let tool_id = reg
        .ids()
        .into_iter()
        .find(|id| id.contains("chart"))
        .expect("chart tool");
    let tool = reg.get(&tool_id).unwrap();

    let ctx = ToolCallContext::test_default();
    let result = tool.execute(json!({}), &ctx).await.unwrap();

    assert!(result.result.is_success());
    assert_eq!(
        result.result.metadata["mcp.ui.content"],
        json!("<html>chart</html>")
    );
    assert_eq!(
        result.result.metadata["mcp.ui.mimeType"],
        json!("text/html")
    );
    assert_eq!(
        result.result.metadata["mcp.ui.resourceUri"],
        json!("ui://chart/render")
    );
}

#[tokio::test]
async fn mcp_tool_execute_ui_fetch_failure_non_fatal() {
    let mut def = McpToolDefinition::new("broken");
    def.meta = Some(json!({"ui": {"resourceUri": "ui://broken/missing"}}));

    let transport = Arc::new(FakeUiTransport::new(vec![def.clone()]));
    let manager = McpToolRegistryManager::from_transports([(
        cfg("s1"),
        transport as Arc<dyn McpToolTransport>,
    )])
    .await
    .unwrap();
    let reg = manager.registry();
    let tool_id = reg
        .ids()
        .into_iter()
        .find(|id| id.contains("broken"))
        .expect("broken tool");
    let tool = reg.get(&tool_id).unwrap();

    let ctx = ToolCallContext::test_default();
    let result = tool.execute(json!({}), &ctx).await.unwrap();

    assert!(result.result.is_success());
    // UI content should not be present when fetch fails
    assert!(!result.result.metadata.contains_key("mcp.ui.content"));
    let status = manager.server_status_snapshot("s1").await.unwrap();
    assert_eq!(
        status.consecutive_failures, 0,
        "optional UI resource hydration failures must not pollute MCP runtime health"
    );
}

#[tokio::test]
async fn mcp_tool_execute_ui_fetch_timeout_non_fatal() {
    let mut def = McpToolDefinition::new("slow_ui");
    def.meta = Some(json!({"ui": {"resourceUri": "ui://slow/render"}}));

    let transport = Arc::new(
        FakeUiTransport::new(vec![def.clone()])
            .with_resource("ui://slow/render", "<html>slow</html>", "text/html")
            .with_read_delay(Duration::from_secs(2)),
    );
    let manager = McpToolRegistryManager::from_transports([(
        cfg("s1"),
        transport as Arc<dyn McpToolTransport>,
    )])
    .await
    .unwrap();
    let reg = manager.registry();
    let tool_id = reg
        .ids()
        .into_iter()
        .find(|id| id.contains("slow_ui"))
        .expect("slow_ui tool");
    let tool = reg.get(&tool_id).unwrap();

    let started = Instant::now();
    let result = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .unwrap();

    assert!(result.result.is_success());
    assert!(!result.result.metadata.contains_key("mcp.ui.content"));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "optional UI hydration should not stall the tool result hot path"
    );
    let status = manager.server_status_snapshot("s1").await.unwrap();
    assert_eq!(status.consecutive_failures, 0);
}

#[tokio::test]
async fn mcp_tool_without_ui_meta_has_no_ui_uri_in_result() {
    let def = McpToolDefinition::new("echo");

    let transport = Arc::new(FakeTransport::new(vec![def])) as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();
    let tool_id = reg
        .ids()
        .into_iter()
        .find(|id| id.contains("echo"))
        .unwrap();
    let tool = reg.get(&tool_id).unwrap();

    let ctx = ToolCallContext::test_default();
    let result = tool.execute(json!({}), &ctx).await.unwrap();
    assert!(!result.result.metadata.contains_key("mcp.ui.resourceUri"));
}

// ── Sampling handler tests ──

#[tokio::test]
async fn sampling_handler_trait_is_object_safe() {
    struct TestHandler;

    #[async_trait]
    impl SamplingHandler for TestHandler {
        async fn handle_create_message(
            &self,
            _params: CreateMessageParams,
        ) -> Result<CreateMessageResult, McpTransportError> {
            use mcp::{Role, SamplingContent};
            Ok(CreateMessageResult {
                role: Role::Assistant,
                content: vec![SamplingContent::Text {
                    text: "test response".to_string(),
                    annotations: None,
                    meta: None,
                }],
                model: "test-model".to_string(),
                stop_reason: Some("end_turn".to_string()),
                meta: None,
            })
        }
    }

    let handler: Arc<dyn SamplingHandler> = Arc::new(TestHandler);
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
}

#[tokio::test]
async fn failing_sampling_handler_returns_error() {
    struct FailHandler;

    #[async_trait]
    impl SamplingHandler for FailHandler {
        async fn handle_create_message(
            &self,
            _params: CreateMessageParams,
        ) -> Result<CreateMessageResult, McpTransportError> {
            Err(McpTransportError::TransportError(
                "LLM unavailable".to_string(),
            ))
        }
    }

    let handler: Arc<dyn SamplingHandler> = Arc::new(FailHandler);
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

    let err = handler
        .handle_create_message(params)
        .await
        .expect_err("handler should fail");
    assert!(err.to_string().contains("LLM unavailable"));
}

// ── McpTool descriptor and execution edge-case tests ──

#[tokio::test]
async fn mcp_tool_descriptor_contains_correct_id_and_name_from_definition() {
    let def = McpToolDefinition::new("my_func").with_title("My Function");
    let transport = Arc::new(FakeTransport::new(vec![def])) as Arc<dyn McpToolTransport>;

    let manager = McpToolRegistryManager::from_transports([(cfg("srv"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();
    let tool_id = reg
        .ids()
        .into_iter()
        .find(|id| id.contains("my_func"))
        .expect("my_func tool should be discovered");
    let tool = reg.get(&tool_id).unwrap();

    let desc = tool.descriptor();
    assert_eq!(desc.id, tool_id);
    assert_eq!(desc.name, "My Function");
    // When title is absent, name falls back to the MCP tool name
    assert!(desc.description.contains("my_func") || !desc.description.is_empty());
}

#[tokio::test]
async fn mcp_tool_descriptor_name_falls_back_to_tool_name_without_title() {
    let def = McpToolDefinition::new("raw_name");
    let transport = Arc::new(FakeTransport::new(vec![def])) as Arc<dyn McpToolTransport>;

    let manager = McpToolRegistryManager::from_transports([(cfg("srv"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();
    let tool_id = reg
        .ids()
        .into_iter()
        .find(|id| id.contains("raw_name"))
        .unwrap();
    let tool = reg.get(&tool_id).unwrap();

    let desc = tool.descriptor();
    // Without a title, the descriptor name should fall back to the MCP tool name
    assert_eq!(desc.name, "raw_name");
}

#[tokio::test]
async fn mcp_tool_returns_error_for_transport_call_failure() {
    #[derive(Debug, Clone)]
    struct FailingCallTransport;

    #[async_trait]
    impl McpToolTransport for FailingCallTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(vec![McpToolDefinition::new("failing_tool")])
        }

        async fn call_tool(
            &self,
            _name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
            _context: awaken_ext_mcp::McpCallContext,
        ) -> Result<CallToolResult, McpTransportError> {
            Err(McpTransportError::TransportError(
                "connection reset".to_string(),
            ))
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }
    }

    let transport = Arc::new(FailingCallTransport) as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("srv"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();
    let tool_id = reg
        .ids()
        .into_iter()
        .find(|id| id.contains("failing_tool"))
        .unwrap();
    let tool = reg.get(&tool_id).unwrap();

    let ctx = ToolCallContext::test_default();
    let err = tool
        .execute(json!({}), &ctx)
        .await
        .expect_err("transport failure should propagate as ToolError");
    let msg = format!("{err}");
    assert!(
        msg.contains("connection reset"),
        "error should contain transport message, got: {msg}"
    );
}

#[tokio::test]
async fn mcp_tool_result_metadata_contains_server_info() {
    let transport = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]))
        as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("my_server"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();
    let tool_id = reg
        .ids()
        .into_iter()
        .find(|id| id.contains("echo"))
        .unwrap();
    let tool = reg.get(&tool_id).unwrap();

    let ctx = ToolCallContext::test_default();
    let result = tool.execute(json!({}), &ctx).await.unwrap();

    assert_eq!(
        result
            .result
            .metadata
            .get("mcp.server")
            .and_then(|v| v.as_str()),
        Some("my_server"),
        "result metadata should contain the MCP server name"
    );
    assert_eq!(
        result
            .result
            .metadata
            .get("mcp.tool")
            .and_then(|v| v.as_str()),
        Some("echo"),
        "result metadata should contain the MCP tool name"
    );
}

#[tokio::test]
async fn mcp_tool_command_is_empty() {
    let transport = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]))
        as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("srv"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();
    let tool_id = reg
        .ids()
        .into_iter()
        .find(|id| id.contains("echo"))
        .unwrap();
    let tool = reg.get(&tool_id).unwrap();

    let ctx = ToolCallContext::test_default();
    let result = tool.execute(json!({}), &ctx).await.unwrap();

    assert!(
        result.command.is_empty(),
        "MCP tools are passive wrappers and should produce an empty command"
    );
}

// ── HTTP transport integration tests ──

#[tokio::test(flavor = "multi_thread")]
async fn connect_http_registry_discovers_tools_and_executes() {
    let seen_requests = Arc::new(std::sync::Mutex::new(Vec::<HttpRequestSpec>::new()));
    let seen_requests_for_handler = Arc::clone(&seen_requests);
    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        seen_requests_for_handler
            .lock()
            .unwrap()
            .push(request.clone());
        let method = request.body["method"].as_str().unwrap_or_default();
        match method {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => HttpResponseSpec::json_with_headers(
                json!({
                    "jsonrpc": "2.0",
                    "id": request.body["id"].clone(),
                    "result": {
                        "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "http-test", "version": "1.0.0"}
                    }
                }),
                vec![("MCP-Session-Id", "test-session")],
            ),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "echo_http",
                        "title": "Echo HTTP",
                        "description": "Echo tool over HTTP",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"message": {"type": "string"}},
                            "required": ["message"]
                        }
                    }]
                }
            })),
            "tools/call" => {
                let token = request.body["params"]["_meta"]["progressToken"].clone();
                let text = request.body["params"]["arguments"]["message"]
                    .as_str()
                    .unwrap_or("ok");
                HttpResponseSpec::sse(format!(
                    "data: {}\n\n\
                     data: {}\n\n",
                    json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/progress",
                        "params": {
                            "progressToken": token,
                            "progress": 1.0,
                            "total": 4.0,
                            "message": "working"
                        }
                    }),
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "content": [{"type":"text", "text": text}]
                        }
                    })
                ))
            }
            _ => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {}
            })),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_s1", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__echo_http"))
        .expect("discover http tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let descriptor = tool.descriptor();
    assert_eq!(
        descriptor
            .metadata
            .get("mcp.transport")
            .and_then(|v| v.as_str()),
        Some("http")
    );

    let ctx = ToolCallContext::test_default();
    let result = tool
        .execute(json!({"message":"hello-http"}), &ctx)
        .await
        .unwrap();
    server.abort();
    assert!(result.result.is_success());

    let seen_requests = seen_requests.lock().unwrap();
    let initialize_request = seen_requests
        .iter()
        .find(|request| request.body["method"] == "initialize")
        .expect("initialize request");
    assert_eq!(
        initialize_request.headers.get("accept").map(String::as_str),
        Some("application/json, text/event-stream")
    );

    let initialized_notification = seen_requests
        .iter()
        .find(|request| request.body["method"] == "notifications/initialized")
        .expect("initialized notification");
    assert_eq!(
        initialized_notification
            .headers
            .get("mcp-session-id")
            .map(String::as_str),
        Some("test-session")
    );

    let tools_list_request = seen_requests
        .iter()
        .find(|request| request.body["method"] == "tools/list")
        .expect("tools/list request");
    assert_eq!(
        tools_list_request
            .headers
            .get("mcp-protocol-version")
            .map(String::as_str),
        Some(mcp::MCP_PROTOCOL_VERSION)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_initialize_accepts_sse_response() {
    let seen_requests = Arc::new(std::sync::Mutex::new(Vec::<HttpRequestSpec>::new()));
    let seen_requests_for_handler = Arc::clone(&seen_requests);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        seen_requests_for_handler
            .lock()
            .unwrap()
            .push(request.clone());

        if request.method == "GET" {
            return HttpResponseSpec::text(405, "background listener disabled");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => HttpResponseSpec::sse_with_headers(
                format!(
                    "id: init-1\ndata: {}\n\n",
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "http-sse-init", "version": "1.0.0"}
                        }
                    })
                ),
                vec![("MCP-Session-Id", "sse-init-session")],
            ),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "sse_init_tool",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_sse_init", endpoint);
    let manager = McpToolRegistryManager::connect([cfg])
        .await
        .expect("SSE initialize response should negotiate successfully");
    let registry = manager.registry();
    assert!(
        registry
            .ids()
            .into_iter()
            .any(|id| id.ends_with("__sse_init_tool")),
        "tool discovered after SSE initialize"
    );

    manager.close_all().await.expect("close manager");
    server.abort();

    let seen_requests = seen_requests.lock().unwrap();
    let initialized = seen_requests
        .iter()
        .find(|request| request.body["method"] == "notifications/initialized")
        .expect("initialized notification");
    assert_eq!(
        initialized
            .headers
            .get("mcp-session-id")
            .map(String::as_str),
        Some("sse-init-session")
    );
    let tools_list = seen_requests
        .iter()
        .find(|request| request.body["method"] == "tools/list")
        .expect("tools/list request");
    assert_eq!(
        tools_list.headers.get("mcp-session-id").map(String::as_str),
        Some("sse-init-session")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_initialize_sse_resumes_after_clean_close_with_last_event_id() {
    let initialize_id = Arc::new(std::sync::Mutex::new(None::<Value>));
    let initialize_id_for_handler = Arc::clone(&initialize_id);
    let resume_last_ids = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let resume_last_ids_for_handler = Arc::clone(&resume_last_ids);
    let seen_requests = Arc::new(std::sync::Mutex::new(Vec::<HttpRequestSpec>::new()));
    let seen_requests_for_handler = Arc::clone(&seen_requests);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        seen_requests_for_handler
            .lock()
            .unwrap()
            .push(request.clone());

        if request.method == "GET" {
            let Some(last_event_id) = request.headers.get("last-event-id").cloned() else {
                return HttpResponseSpec::text(405, "background listener disabled");
            };
            resume_last_ids_for_handler
                .lock()
                .unwrap()
                .push(last_event_id);
            assert_eq!(
                request.headers.get("mcp-session-id").map(String::as_str),
                Some("init-resume-session")
            );
            assert_eq!(
                request
                    .headers
                    .get("mcp-protocol-version")
                    .map(String::as_str),
                Some(mcp::MCP_PROTOCOL_VERSION),
                "initialize resume GET should carry the client protocol version"
            );
            let request_id = initialize_id_for_handler
                .lock()
                .unwrap()
                .clone()
                .expect("initialize id captured before resume");
            return HttpResponseSpec::sse(format!(
                "id: init-2\ndata: {}\n\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "http-sse-init-resume", "version": "1.0.0"}
                    }
                })
            ));
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => {
                *initialize_id_for_handler.lock().unwrap() = Some(request.body["id"].clone());
                HttpResponseSpec::sse_with_headers(
                    "id: init-1\nretry: 1\ndata:\n\n",
                    vec![("MCP-Session-Id", "init-resume-session")],
                )
            }
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "sse_init_resume_tool",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_sse_init_resume", endpoint);
    let manager = McpToolRegistryManager::connect([cfg])
        .await
        .expect("SSE initialize response should resume through Last-Event-ID");
    let registry = manager.registry();
    assert!(
        registry
            .ids()
            .into_iter()
            .any(|id| id.ends_with("__sse_init_resume_tool")),
        "tool discovered after resumed SSE initialize"
    );

    manager.close_all().await.expect("close manager");
    server.abort();

    assert_eq!(
        resume_last_ids.lock().unwrap().as_slice(),
        &["init-1".to_string()]
    );
    let seen_requests = seen_requests.lock().unwrap();
    let initialized = seen_requests
        .iter()
        .find(|request| request.body["method"] == "notifications/initialized")
        .expect("initialized notification");
    assert_eq!(
        initialized
            .headers
            .get("mcp-session-id")
            .map(String::as_str),
        Some("init-resume-session")
    );
    assert_eq!(
        initialized
            .headers
            .get("mcp-protocol-version")
            .map(String::as_str),
        Some(mcp::MCP_PROTOCOL_VERSION)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_streamable_without_session_uses_protocol_header_and_no_delete() {
    let seen_requests = Arc::new(std::sync::Mutex::new(Vec::<HttpRequestSpec>::new()));
    let seen_requests_for_handler = Arc::clone(&seen_requests);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        seen_requests_for_handler
            .lock()
            .unwrap()
            .push(request.clone());

        if request.method == "GET" {
            return HttpResponseSpec::text(405, "stateless listener disabled");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(500, "stateless close must not DELETE");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "stateless-http", "version": "1.0.0"}
                }
            })),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "stateless_echo",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "content": [{"type": "text", "text": "stateless ok"}]
                }
            })),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_stateless", endpoint);
    let manager = McpToolRegistryManager::connect([cfg])
        .await
        .expect("stateless HTTP initialize should connect");
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__stateless_echo"))
        .expect("discover stateless_echo tool");
    let tool = registry.get(&tool_id).expect("registry tool");
    let result = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect("stateless tool call succeeds");

    manager.close_all().await.expect("close manager");
    server.abort();

    assert_eq!(result.result.data, json!("stateless ok"));
    let seen_requests = seen_requests.lock().unwrap();
    assert!(
        !seen_requests
            .iter()
            .any(|request| request.method == "DELETE"),
        "stateless close must not send DELETE without a server session id"
    );

    for method in ["notifications/initialized", "tools/list", "tools/call"] {
        let request = seen_requests
            .iter()
            .find(|request| request.body["method"] == method)
            .unwrap_or_else(|| panic!("missing request for {method}"));
        assert!(
            !request.headers.contains_key("mcp-session-id"),
            "{method} must not carry MCP-Session-Id when server did not assign one"
        );
        assert_eq!(
            request
                .headers
                .get("mcp-protocol-version")
                .map(String::as_str),
            Some(mcp::MCP_PROTOCOL_VERSION),
            "{method} must carry negotiated MCP-Protocol-Version"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn http_initialize_unsupported_protocol_deletes_provisional_session() {
    let seen_requests = Arc::new(std::sync::Mutex::new(Vec::<HttpRequestSpec>::new()));
    let seen_requests_for_handler = Arc::clone(&seen_requests);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        seen_requests_for_handler
            .lock()
            .unwrap()
            .push(request.clone());

        if request.method == "GET" {
            return HttpResponseSpec::text(405, "background listener disabled");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "initialize" => HttpResponseSpec::json_with_headers(
                json!({
                    "jsonrpc": "2.0",
                    "id": request.body["id"].clone(),
                    "result": {
                        "protocolVersion": "1900-01-01",
                        "capabilities": {},
                        "serverInfo": {"name": "bad-protocol", "version": "1.0.0"}
                    }
                }),
                vec![("MCP-Session-Id", "unsupported-session")],
            ),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_init_bad_protocol", endpoint);
    let err = McpToolRegistryManager::connect([cfg])
        .await
        .expect_err("unsupported protocol should fail initialize");

    assert!(
        format!("{err}").contains("unsupported protocolVersion"),
        "unexpected error: {err}"
    );
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            seen_requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.method == "DELETE")
        })
        .await,
        "initialize failure should DELETE the provisional session"
    );
    server.abort();

    let seen_requests = seen_requests.lock().unwrap();
    let delete_requests: Vec<_> = seen_requests
        .iter()
        .filter(|request| request.method == "DELETE")
        .collect();
    assert_eq!(delete_requests.len(), 1);
    assert_eq!(
        delete_requests[0]
            .headers
            .get("mcp-session-id")
            .map(String::as_str),
        Some("unsupported-session")
    );
    assert!(
        !seen_requests
            .iter()
            .any(|request| request.body["method"] == "notifications/initialized"),
        "initialized notification must not be sent after protocol negotiation failure"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_initialize_notification_failure_deletes_provisional_session() {
    let seen_requests = Arc::new(std::sync::Mutex::new(Vec::<HttpRequestSpec>::new()));
    let seen_requests_for_handler = Arc::clone(&seen_requests);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        seen_requests_for_handler
            .lock()
            .unwrap()
            .push(request.clone());

        if request.method == "GET" {
            return HttpResponseSpec::text(405, "background listener disabled");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "initialize" => HttpResponseSpec::json_with_headers(
                json!({
                    "jsonrpc": "2.0",
                    "id": request.body["id"].clone(),
                    "result": {
                        "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                        "capabilities": {},
                        "serverInfo": {"name": "init-notify-fail", "version": "1.0.0"}
                    }
                }),
                vec![("MCP-Session-Id", "notify-fail-session")],
            ),
            "notifications/initialized" => HttpResponseSpec::text(400, "initialized rejected"),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_init_notify_fail", endpoint);
    let err = McpToolRegistryManager::connect([cfg])
        .await
        .expect_err("initialized notification failure should fail initialize");

    assert!(
        format!("{err}").contains("initialized rejected"),
        "unexpected error: {err}"
    );
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            seen_requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.method == "DELETE")
        })
        .await,
        "initialized failure should DELETE the provisional session"
    );
    server.abort();

    let seen_requests = seen_requests.lock().unwrap();
    let initialized = seen_requests
        .iter()
        .find(|request| request.body["method"] == "notifications/initialized")
        .expect("initialized notification request");
    assert_eq!(
        initialized
            .headers
            .get("mcp-session-id")
            .map(String::as_str),
        Some("notify-fail-session")
    );
    let delete_requests: Vec<_> = seen_requests
        .iter()
        .filter(|request| request.method == "DELETE")
        .collect();
    assert_eq!(delete_requests.len(), 1);
    assert_eq!(
        delete_requests[0]
            .headers
            .get("mcp-session-id")
            .map(String::as_str),
        Some("notify-fail-session")
    );
    assert_eq!(
        delete_requests[0]
            .headers
            .get("mcp-protocol-version")
            .map(String::as_str),
        Some(mcp::MCP_PROTOCOL_VERSION)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_per_request_sse_resumes_after_clean_close_with_last_event_id() {
    let tools_call_id = Arc::new(std::sync::Mutex::new(None::<Value>));
    let resume_last_ids = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let get_count = Arc::new(AtomicUsize::new(0));
    let tools_call_id_for_handler = Arc::clone(&tools_call_id);
    let resume_last_ids_for_handler = Arc::clone(&resume_last_ids);
    let get_count_for_handler = Arc::clone(&get_count);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            get_count_for_handler.fetch_add(1, Ordering::SeqCst);
            let Some(last_event_id) = request.headers.get("last-event-id").cloned() else {
                return HttpResponseSpec::text(405, "background listener disabled");
            };
            resume_last_ids_for_handler
                .lock()
                .unwrap()
                .push(last_event_id);
            let request_id = tools_call_id_for_handler
                .lock()
                .unwrap()
                .clone()
                .expect("tools/call id captured before resume");
            return HttpResponseSpec::sse(format!(
                "id: 2\ndata: {}\n\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "content": [{"type": "text", "text": "resumed ok"}]
                    }
                })
            ));
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"tools": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "resume_http",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => {
                *tools_call_id_for_handler.lock().unwrap() = Some(request.body["id"].clone());
                let token = request.body["params"]["_meta"]["progressToken"].clone();
                HttpResponseSpec::sse(format!(
                    "id: 1\nretry: 1\ndata: {}\n\n",
                    json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/progress",
                        "params": {
                            "progressToken": token,
                            "progress": 1.0,
                            "message": "halfway"
                        }
                    })
                ))
            }
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_sse_resume", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__resume_http"))
        .expect("discover resume tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let result = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect("SSE response should resume after clean close");

    server.abort();
    assert!(result.result.is_success());
    assert_eq!(result.result.data, json!("resumed ok"));
    assert!(get_count.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        resume_last_ids.lock().unwrap().clone(),
        vec!["1".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_per_request_sse_progress_can_exceed_request_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw http listener");
    let addr = listener.local_addr().expect("listener addr");
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Some(request) = read_http_request(&mut stream).await else {
                    return;
                };
                if request.method == "GET" {
                    write_http_response(
                        &mut stream,
                        HttpResponseSpec::text(405, "background listener disabled"),
                    )
                    .await;
                    return;
                }

                match request.body["method"].as_str().unwrap_or_default() {
                    "notifications/initialized" => {
                        write_http_response(&mut stream, HttpResponseSpec::accepted()).await;
                    }
                    "initialize" => {
                        write_http_response(
                            &mut stream,
                            initialize_response(&request, json!({"tools": {}})),
                        )
                        .await;
                    }
                    "tools/list" => {
                        write_http_response(
                            &mut stream,
                            HttpResponseSpec::json(json!({
                                "jsonrpc": "2.0",
                                "id": request.body["id"].clone(),
                                "result": {
                                    "tools": [{
                                        "name": "long_sse",
                                        "inputSchema": {"type": "object", "properties": {}}
                                    }]
                                }
                            })),
                        )
                        .await;
                    }
                    "tools/call" => {
                        let head = concat!(
                            "HTTP/1.1 200 OK\r\n",
                            "Content-Type: text/event-stream\r\n",
                            "Connection: close\r\n\r\n"
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        for _ in 0..5 {
                            let _ = stream.write_all(b": progress\n\n").await;
                            let _ = stream.flush().await;
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                        let final_event = format!(
                            "data: {}\n\n",
                            json!({
                                "jsonrpc": "2.0",
                                "id": request.body["id"].clone(),
                                "result": {
                                    "content": [{"type": "text", "text": "long ok"}]
                                }
                            })
                        );
                        let _ = stream.write_all(final_event.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    }
                    other => panic!("unexpected method: {other}"),
                }
            });
        }
    });

    let mut cfg = McpServerConnectionConfig::http("http_long_sse", format!("http://{addr}"));
    cfg.timeout_secs = 1;
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__long_sse"))
        .expect("discover long_sse tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        tool.execute(json!({}), &ToolCallContext::test_default()),
    )
    .await
    .expect("stream should outlive request timeout without hanging")
    .expect("long SSE should succeed");

    manager.close_all().await.expect("close manager");
    server.abort();

    assert_eq!(result.result.data, json!("long ok"));
}

#[tokio::test(flavor = "multi_thread")]
async fn http_per_request_sse_rejects_oversized_line() {
    let oversized = "x".repeat(70 * 1024);
    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            return HttpResponseSpec::text(405, "background listener disabled");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"tools": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "oversized_sse",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => HttpResponseSpec::sse(format!("data: {oversized}\n\n")),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_oversized_sse", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__oversized_sse"))
        .expect("discover oversized_sse tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let err = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect_err("oversized SSE line must be rejected");

    manager.close_all().await.expect("close manager");
    server.abort();

    assert!(
        format!("{err}").contains("SSE line exceeded"),
        "unexpected error: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_per_request_sse_rejects_retry_delay_above_limit() {
    let resume_get_count = Arc::new(AtomicUsize::new(0));
    let resume_get_count_for_handler = Arc::clone(&resume_get_count);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            if request.headers.contains_key("last-event-id") {
                resume_get_count_for_handler.fetch_add(1, Ordering::SeqCst);
                return HttpResponseSpec::text(500, "oversized retry should not resume");
            }
            return HttpResponseSpec::text(405, "background listener disabled");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"tools": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "huge_retry_sse",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => HttpResponseSpec::sse("id: 1\nretry: 86400000\ndata:\n\n"),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_huge_retry_sse", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__huge_retry_sse"))
        .expect("discover huge_retry_sse tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let err = tokio::time::timeout(
        Duration::from_secs(1),
        tool.execute(json!({}), &ToolCallContext::test_default()),
    )
    .await
    .expect("oversized retry must fail without sleeping for the server delay")
    .expect_err("oversized retry delay must be rejected");

    manager.close_all().await.expect("close manager");
    server.abort();

    assert!(
        format!("{err}").contains("SSE retry delay exceeded"),
        "unexpected error: {err}"
    );
    assert_eq!(
        resume_get_count.load(Ordering::SeqCst),
        0,
        "client must reject the retry before issuing Last-Event-ID resume"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_per_request_sse_ignores_retry_delay_when_final_response_arrives() {
    let resume_get_count = Arc::new(AtomicUsize::new(0));
    let resume_get_count_for_handler = Arc::clone(&resume_get_count);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            if request.headers.contains_key("last-event-id") {
                resume_get_count_for_handler.fetch_add(1, Ordering::SeqCst);
            }
            return HttpResponseSpec::text(405, "background listener disabled");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"tools": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "final_with_retry",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => HttpResponseSpec::sse(format!(
                "id: 1\nretry: 86400000\ndata: {}\n\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": request.body["id"].clone(),
                    "result": {
                        "content": [{"type": "text", "text": "final ok"}]
                    }
                })
            )),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_final_with_retry", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__final_with_retry"))
        .expect("discover final_with_retry tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let result = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect("final response should not be rejected by an unused retry field");

    manager.close_all().await.expect("close manager");
    server.abort();

    assert_eq!(result.result.data, json!("final ok"));
    assert_eq!(
        resume_get_count.load(Ordering::SeqCst),
        0,
        "final response should complete without Last-Event-ID resume"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_sse_resume_404_after_accepted_call_is_not_replayed() {
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let initialize_count_for_handler = Arc::clone(&initialize_count);
    let tools_call_count = Arc::new(AtomicUsize::new(0));
    let tools_call_count_for_handler = Arc::clone(&tools_call_count);
    let resume_get_count = Arc::new(AtomicUsize::new(0));
    let resume_get_count_for_handler = Arc::clone(&resume_get_count);
    let tool_call_sessions = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let tool_call_sessions_for_handler = Arc::clone(&tool_call_sessions);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            if request.headers.contains_key("last-event-id") {
                resume_get_count_for_handler.fetch_add(1, Ordering::SeqCst);
                return HttpResponseSpec::text(404, "session expired");
            }
            return HttpResponseSpec::text(405, "background listener disabled");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => {
                let n = initialize_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                HttpResponseSpec::json_with_headers(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "test-server", "version": "1.0.0"}
                        }
                    }),
                    vec![("MCP-Session-Id", format!("session-{n}"))],
                )
            }
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "resume_404",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => {
                let n = tools_call_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                tool_call_sessions_for_handler
                    .lock()
                    .unwrap()
                    .push(request.headers.get("mcp-session-id").cloned());
                if n == 1 {
                    HttpResponseSpec::sse(format!(
                        "id: 1\ndata: {}\n\n",
                        json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {"progressToken": "p", "progress": 1}
                        })
                    ))
                } else {
                    HttpResponseSpec::json(json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "content": [{"type": "text", "text": "retry ok"}]
                        }
                    }))
                }
            }
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_sse_resume_404", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__resume_404"))
        .expect("discover resume_404 tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let err = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect_err("resume 404 after an accepted SSE call must not replay tools/call");

    server.abort();

    assert!(
        format!("{err}").contains("request was accepted"),
        "accepted-call resume failure should surface without silent replay, got: {err}"
    );
    assert_eq!(initialize_count.load(Ordering::SeqCst), 1);
    assert_eq!(resume_get_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        tool_call_sessions.lock().unwrap().as_slice(),
        &[Some("session-1".to_string())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_sse_resume_404_after_body_io_error_is_not_replayed() {
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let tools_call_count = Arc::new(AtomicUsize::new(0));
    let resume_get_count = Arc::new(AtomicUsize::new(0));
    let tool_call_sessions = Arc::new(Mutex::new(Vec::<Option<String>>::new()));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw http listener");
    let addr = listener.local_addr().expect("listener addr");
    let initialize_count_for_server = Arc::clone(&initialize_count);
    let tools_call_count_for_server = Arc::clone(&tools_call_count);
    let resume_get_count_for_server = Arc::clone(&resume_get_count);
    let tool_call_sessions_for_server = Arc::clone(&tool_call_sessions);
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let initialize_count = Arc::clone(&initialize_count_for_server);
            let tools_call_count = Arc::clone(&tools_call_count_for_server);
            let resume_get_count = Arc::clone(&resume_get_count_for_server);
            let tool_call_sessions = Arc::clone(&tool_call_sessions_for_server);
            tokio::spawn(async move {
                let Some(request) = read_http_request(&mut stream).await else {
                    return;
                };
                if request.method == "GET" {
                    let response = if request.headers.contains_key("last-event-id") {
                        resume_get_count.fetch_add(1, Ordering::SeqCst);
                        HttpResponseSpec::text(404, "session expired")
                    } else {
                        HttpResponseSpec::text(405, "background listener disabled")
                    };
                    write_http_response(&mut stream, response).await;
                    return;
                }

                match request.body["method"].as_str().unwrap_or_default() {
                    "notifications/initialized" => {
                        write_http_response(&mut stream, HttpResponseSpec::accepted()).await;
                    }
                    "initialize" => {
                        let n = initialize_count.fetch_add(1, Ordering::SeqCst) + 1;
                        write_http_response(
                            &mut stream,
                            HttpResponseSpec::json_with_headers(
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": request.body["id"].clone(),
                                    "result": {
                                        "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                                        "capabilities": {"tools": {}},
                                        "serverInfo": {"name": "test-server", "version": "1.0.0"}
                                    }
                                }),
                                vec![("MCP-Session-Id", format!("session-{n}"))],
                            ),
                        )
                        .await;
                    }
                    "tools/list" => {
                        write_http_response(
                            &mut stream,
                            HttpResponseSpec::json(json!({
                                "jsonrpc": "2.0",
                                "id": request.body["id"].clone(),
                                "result": {
                                    "tools": [{
                                        "name": "io_error_resume_404",
                                        "inputSchema": {"type": "object", "properties": {}}
                                    }]
                                }
                            })),
                        )
                        .await;
                    }
                    "tools/call" => {
                        let n = tools_call_count.fetch_add(1, Ordering::SeqCst) + 1;
                        tool_call_sessions
                            .lock()
                            .unwrap()
                            .push(request.headers.get("mcp-session-id").cloned());
                        if n == 1 {
                            let payload = format!(
                                "id: 1\ndata: {}\n\n",
                                json!({
                                    "jsonrpc": "2.0",
                                    "method": "notifications/progress",
                                    "params": {"progressToken": "p", "progress": 1}
                                })
                            );
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                payload.len() + 1024
                            );
                            let _ = stream.write_all(head.as_bytes()).await;
                            let _ = stream.write_all(payload.as_bytes()).await;
                            let _ = stream.shutdown().await;
                        } else {
                            write_http_response(
                                &mut stream,
                                HttpResponseSpec::json(json!({
                                    "jsonrpc": "2.0",
                                    "id": request.body["id"].clone(),
                                    "result": {
                                        "content": [{"type": "text", "text": "retry ok"}]
                                    }
                                })),
                            )
                            .await;
                        }
                    }
                    other => panic!("unexpected method: {other}"),
                }
            });
        }
    });

    let cfg =
        McpServerConnectionConfig::http("http_sse_io_error_resume_404", format!("http://{addr}"));
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__io_error_resume_404"))
        .expect("discover io_error_resume_404 tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let err = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect_err("resume 404 after an accepted streaming body must not replay tools/call");

    server.abort();

    assert!(
        format!("{err}").contains("request was accepted"),
        "accepted-call resume failure should surface without silent replay, got: {err}"
    );
    assert_eq!(initialize_count.load(Ordering::SeqCst), 1);
    assert_eq!(resume_get_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        tool_call_sessions.lock().unwrap().as_slice(),
        &[Some("session-1".to_string())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_sse_resume_after_session_cleared_is_not_replayed() {
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let initialize_count_for_handler = Arc::clone(&initialize_count);
    let tools_call_count = Arc::new(AtomicUsize::new(0));
    let tools_call_count_for_handler = Arc::clone(&tools_call_count);
    let background_get_count = Arc::new(AtomicUsize::new(0));
    let background_get_count_for_handler = Arc::clone(&background_get_count);
    let resume_get_count = Arc::new(AtomicUsize::new(0));
    let resume_get_count_for_handler = Arc::clone(&resume_get_count);
    let release_background_404 = Arc::new(AtomicBool::new(false));
    let release_background_404_for_handler = Arc::clone(&release_background_404);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            if request.headers.contains_key("last-event-id") {
                resume_get_count_for_handler.fetch_add(1, Ordering::SeqCst);
                return HttpResponseSpec::text(500, "resume GET should not be sent");
            }
            let n = background_get_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                while !release_background_404_for_handler.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                return HttpResponseSpec::text(404, "session expired");
            }
            return HttpResponseSpec::text(405, "background listener disabled");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => {
                let n = initialize_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                HttpResponseSpec::json_with_headers(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "test-server", "version": "1.0.0"}
                        }
                    }),
                    vec![("MCP-Session-Id", format!("session-{n}"))],
                )
            }
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "resume_without_session",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => {
                let n = tools_call_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    HttpResponseSpec::sse(format!(
                        "id: 1\nretry: 300\ndata: {}\n\n",
                        json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {"progressToken": "p", "progress": 1}
                        })
                    ))
                } else {
                    HttpResponseSpec::json(json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "content": [{"type": "text", "text": "retry ok"}]
                        }
                    }))
                }
            }
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_resume_without_session", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            background_get_count.load(Ordering::SeqCst) >= 1
        })
        .await,
        "background GET should be in flight"
    );
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__resume_without_session"))
        .expect("discover resume_without_session tool");
    let tool = registry.get(&tool_id).expect("registry tool");
    let call = tokio::spawn(async move {
        tool.execute(json!({}), &ToolCallContext::test_default())
            .await
    });
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            tools_call_count.load(Ordering::SeqCst) >= 1
        })
        .await,
        "first tools/call should enter SSE resume path"
    );
    release_background_404.store(true, Ordering::SeqCst);
    let err = call
        .await
        .expect("tool task joins")
        .expect_err("session-cleared accepted SSE call must not be replayed");

    manager.close_all().await.expect("close manager");
    server.abort();

    assert!(
        format!("{err}").contains("request was accepted"),
        "accepted-call resume failure should surface without silent replay, got: {err}"
    );
    assert_eq!(
        resume_get_count.load(Ordering::SeqCst),
        0,
        "resume path must not send Last-Event-ID GET without a negotiated session"
    );
    assert_eq!(initialize_count.load(Ordering::SeqCst), 1);
    assert_eq!(tools_call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn http_sse_resume_after_reinitialize_is_not_replayed() {
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let initialize_count_for_handler = Arc::clone(&initialize_count);
    let primary_started = Arc::new(AtomicBool::new(false));
    let primary_started_for_handler = Arc::clone(&primary_started);
    let resetter_expired = Arc::new(AtomicBool::new(false));
    let resetter_expired_for_handler = Arc::clone(&resetter_expired);
    let tool_call_sessions = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
    let tool_call_sessions_for_handler = Arc::clone(&tool_call_sessions);
    let resume_get_headers = Arc::new(Mutex::new(
        Vec::<std::collections::HashMap<String, String>>::new(),
    ));
    let resume_get_headers_for_handler = Arc::clone(&resume_get_headers);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            if request.headers.contains_key("last-event-id") {
                resume_get_headers_for_handler
                    .lock()
                    .unwrap()
                    .push(request.headers.clone());
                return HttpResponseSpec::text(500, "cross-session resume GET forbidden");
            }
            return HttpResponseSpec::text(405, "background listener disabled");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => {
                let n = initialize_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                HttpResponseSpec::json_with_headers(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "test-server", "version": "1.0.0"}
                        }
                    }),
                    vec![("MCP-Session-Id", format!("session-{n}"))],
                )
            }
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "cross_session_resume",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"message": {"type": "string"}}
                        }
                    }]
                }
            })),
            "tools/call" => {
                let message = request.body["params"]["arguments"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                tool_call_sessions_for_handler.lock().unwrap().push((
                    message.clone(),
                    request.headers.get("mcp-session-id").cloned(),
                ));

                if message == "primary" && !primary_started_for_handler.swap(true, Ordering::SeqCst)
                {
                    return HttpResponseSpec::sse("id: old-1\nretry: 1000\n\n");
                }
                if message == "resetter"
                    && !resetter_expired_for_handler.swap(true, Ordering::SeqCst)
                {
                    return HttpResponseSpec::text(404, "session gone");
                }

                HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request.body["id"].clone(),
                    "result": {
                        "content": [{"type": "text", "text": format!("{message} ok")}]
                    }
                }))
            }
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_cross_session_resume", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__cross_session_resume"))
        .expect("discover cross_session_resume tool");
    let primary_tool = registry.get(&tool_id).expect("registry tool");
    let resetter_tool = registry.get(&tool_id).expect("registry tool");

    let primary = tokio::spawn(async move {
        primary_tool
            .execute(
                json!({"message": "primary"}),
                &ToolCallContext::test_default(),
            )
            .await
    });
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            primary_started.load(Ordering::SeqCst)
        })
        .await,
        "primary call should enter SSE resume sleep with old-1 cursor"
    );

    let resetter = resetter_tool
        .execute(
            json!({"message": "resetter"}),
            &ToolCallContext::test_default(),
        )
        .await
        .expect("resetter should reinitialize after 404");
    assert_eq!(resetter.result.data, json!("resetter ok"));
    assert_eq!(initialize_count.load(Ordering::SeqCst), 2);

    let primary_err = primary
        .await
        .expect("primary task joins")
        .expect_err("accepted primary SSE call must not replay after another call reinitializes");

    manager.close_all().await.expect("close manager");
    server.abort();

    assert!(
        format!("{primary_err}").contains("request was accepted"),
        "accepted-call resume failure should surface without silent replay, got: {primary_err}"
    );
    assert!(
        resume_get_headers.lock().unwrap().is_empty(),
        "resume path must not send session-2 + old Last-Event-ID: {:?}",
        resume_get_headers.lock().unwrap()
    );
    assert_eq!(
        tool_call_sessions.lock().unwrap().as_slice(),
        &[
            ("primary".to_string(), Some("session-1".to_string())),
            ("resetter".to_string(), Some("session-1".to_string())),
            ("resetter".to_string(), Some("session-2".to_string())),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_cancelled_notification_uses_original_session_after_reinitialize() {
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let initialize_count_for_handler = Arc::clone(&initialize_count);
    let resetter_expired = Arc::new(AtomicBool::new(false));
    let resetter_expired_for_handler = Arc::clone(&resetter_expired);
    let slow_started = Arc::new(AtomicBool::new(false));
    let slow_started_for_handler = Arc::clone(&slow_started);
    let tool_call_sessions = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
    let tool_call_sessions_for_handler = Arc::clone(&tool_call_sessions);
    let cancel_sessions = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let cancel_sessions_for_handler = Arc::clone(&cancel_sessions);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            return HttpResponseSpec::text(405, "background listener disabled");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "notifications/cancelled" => {
                cancel_sessions_for_handler
                    .lock()
                    .unwrap()
                    .push(request.headers.get("mcp-session-id").cloned());
                HttpResponseSpec::accepted()
            }
            "initialize" => {
                let n = initialize_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                HttpResponseSpec::json_with_headers(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "test-server", "version": "1.0.0"}
                        }
                    }),
                    vec![("MCP-Session-Id", format!("session-{n}"))],
                )
            }
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "cancel_session_race",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"message": {"type": "string"}}
                        }
                    }]
                }
            })),
            "tools/call" => {
                let message = request.body["params"]["arguments"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                tool_call_sessions_for_handler.lock().unwrap().push((
                    message.clone(),
                    request.headers.get("mcp-session-id").cloned(),
                ));

                if message == "slow" {
                    slow_started_for_handler.store(true, Ordering::SeqCst);
                    return HttpResponseSpec::sse("id: slow-1\nretry: 5000\ndata:\n\n");
                }
                if message == "resetter"
                    && !resetter_expired_for_handler.swap(true, Ordering::SeqCst)
                {
                    return HttpResponseSpec::text(404, "session gone");
                }

                HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request.body["id"].clone(),
                    "result": {
                        "content": [{"type": "text", "text": format!("{message} ok")}]
                    }
                }))
            }
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_cancel_session_race", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__cancel_session_race"))
        .expect("discover cancel_session_race tool");
    let slow_tool = registry.get(&tool_id).expect("registry tool");
    let resetter_tool = registry.get(&tool_id).expect("registry tool");

    let token = CancellationToken::new();
    let mut slow_ctx = ToolCallContext::test_default();
    slow_ctx.cancellation_token = Some(token.clone());
    let slow = tokio::spawn(async move {
        slow_tool
            .execute(json!({"message": "slow"}), &slow_ctx)
            .await
    });
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            slow_started.load(Ordering::SeqCst)
        })
        .await,
        "slow call should enter per-request SSE under session-1"
    );

    let resetter = resetter_tool
        .execute(
            json!({"message": "resetter"}),
            &ToolCallContext::test_default(),
        )
        .await
        .expect("resetter should reinitialize after POST 404");
    assert_eq!(resetter.result.data, json!("resetter ok"));
    assert_eq!(initialize_count.load(Ordering::SeqCst), 2);

    token.cancel();
    let slow_err = slow
        .await
        .expect("slow task joins")
        .expect_err("slow call should return cancellation");
    assert!(
        format!("{slow_err}").contains("cancel"),
        "expected cancellation error, got: {slow_err}"
    );

    manager.close_all().await.expect("close manager");
    server.abort();

    assert_eq!(
        cancel_sessions.lock().unwrap().as_slice(),
        &[Some("session-1".to_string())],
        "cancellation must use the original tools/call session, not session-2"
    );
    assert_eq!(
        tool_call_sessions.lock().unwrap().as_slice(),
        &[
            ("slow".to_string(), Some("session-1".to_string())),
            ("resetter".to_string(), Some("session-1".to_string())),
            ("resetter".to_string(), Some("session-2".to_string())),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_sse_final_event_without_newline_is_processed() {
    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            return HttpResponseSpec::text(405, "background listener disabled");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"tools": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "unterminated_sse",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => HttpResponseSpec::sse(format!(
                "data: {}",
                json!({
                    "jsonrpc": "2.0",
                    "id": request.body["id"].clone(),
                    "result": {
                        "content": [{"type": "text", "text": "unterminated ok"}]
                    }
                })
            )),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_unterminated_sse", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__unterminated_sse"))
        .expect("discover unterminated_sse tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let result = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect("unterminated final SSE event should be processed");

    server.abort();

    assert_eq!(result.result.data, json!("unterminated ok"));
}

#[tokio::test(flavor = "multi_thread")]
async fn http_sse_id_reset_clears_resume_cursor() {
    let resume_last_ids = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let resume_last_ids_for_handler = Arc::clone(&resume_last_ids);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            if let Some(last_event_id) = request.headers.get("last-event-id").cloned() {
                resume_last_ids_for_handler
                    .lock()
                    .unwrap()
                    .push(last_event_id);
            }
            return HttpResponseSpec::text(405, "no resumable cursor");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"tools": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "reset_cursor",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => HttpResponseSpec::sse(format!(
                "id: 1\ndata: {}\n\nid:\ndata: {}\n\n",
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": request.body["params"]["_meta"]["progressToken"].clone(),
                        "progress": 1.0
                    }
                }),
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": request.body["params"]["_meta"]["progressToken"].clone(),
                        "progress": 2.0
                    }
                })
            )),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_sse_id_reset", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__reset_cursor"))
        .expect("discover reset cursor tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let err = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect_err("missing final response after id reset should fail without resume");

    server.abort();
    assert!(
        format!("{err}").contains("Missing response"),
        "unexpected error: {err}"
    );
    assert!(
        resume_last_ids.lock().unwrap().is_empty(),
        "empty id: must clear stale Last-Event-ID, got {:?}",
        resume_last_ids.lock().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_sse_resume_rejects_non_sse_content_type() {
    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            if request.headers.contains_key("last-event-id") {
                return HttpResponseSpec::json(json!({"status": "not-sse"}));
            }
            return HttpResponseSpec::text(405, "background listener disabled");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"tools": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "bad_resume_type",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => HttpResponseSpec::sse(format!(
                "id: 1\ndata: {}\n\n",
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": request.body["params"]["_meta"]["progressToken"].clone(),
                        "progress": 1.0
                    }
                })
            )),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_bad_resume_type", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__bad_resume_type"))
        .expect("discover bad resume tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let err = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect_err("resume GET with JSON content-type must fail");

    server.abort();
    assert!(
        format!("{err}").contains("expected Content-Type text/event-stream"),
        "unexpected error: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_sse_resume_rejects_response_for_different_request_id() {
    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            if request.headers.contains_key("last-event-id") {
                return HttpResponseSpec::sse(format!(
                    "id: 2\ndata: {}\n\n",
                    json!({
                        "jsonrpc": "2.0",
                        "id": 999,
                        "result": {
                            "content": [{"type": "text", "text": "wrong stream"}]
                        }
                    })
                ));
            }
            return HttpResponseSpec::text(405, "background listener disabled");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"tools": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "wrong_resume",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => HttpResponseSpec::sse(format!(
                "id: 1\ndata: {}\n\n",
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": request.body["params"]["_meta"]["progressToken"].clone(),
                        "progress": 1.0
                    }
                })
            )),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_wrong_resume", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__wrong_resume"))
        .expect("discover wrong resume tool");
    let tool = registry.get(&tool_id).expect("registry tool");

    let err = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect_err("wrong response id on resumed stream must fail");

    server.abort();
    assert!(
        format!("{err}").contains("returned response for different id"),
        "unexpected error: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn manager_close_all_sends_http_delete_for_live_session() {
    let delete_sessions = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let delete_sessions_for_handler = Arc::clone(&delete_sessions);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            return HttpResponseSpec::text(405, "no listening stream");
        }
        if request.method == "DELETE" {
            delete_sessions_for_handler
                .lock()
                .unwrap()
                .push(request.headers["mcp-session-id"].clone());
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"tools": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "echo",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_close_all", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();

    manager.close_all().await.expect("close all transports");
    server.abort();

    assert_eq!(
        delete_sessions.lock().unwrap().clone(),
        vec!["test-session".to_string()]
    );
    assert!(manager.registry().ids().is_empty());
}

#[tokio::test]
async fn http_non_success_status_is_reported() {
    let (endpoint, server) = spawn_http_server(Arc::new(|request| {
        if request.body["method"] == "notifications/initialized" {
            HttpResponseSpec::accepted()
        } else {
            HttpResponseSpec::text(500, "upstream error")
        }
    }))
    .await;
    let cfg = McpServerConnectionConfig::http("http_error_status", endpoint);
    let err = McpToolRegistryManager::connect([cfg])
        .await
        .expect_err("error");
    server.abort();
    assert!(matches!(err, McpError::Transport(_)));
}

/// R9 #3 / R10 #5 regression: after initialize, the HTTP transport
/// opens a background GET listening stream so the server can push
/// notifications (and requests) that aren't tied to a POST response.
/// We assert the full chain end-to-end: a server-pushed
/// `notifications/tools/list_changed` on the GET stream drives the
/// manager's watcher to re-run `tools/list`. Before R9, no GET
/// listener existed and the notification was unreachable during idle
/// periods.
#[tokio::test]
async fn http_listening_stream_drives_tools_refresh() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tools_list_count = Arc::new(AtomicUsize::new(0));
    let get_count = Arc::new(AtomicUsize::new(0));
    let tools_list_count_clone = Arc::clone(&tools_list_count);
    let get_count_clone = Arc::clone(&get_count);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        // GET = the background listening stream that R9 #3 added.
        // First GET responds with a single list_changed SSE event,
        // then closes; subsequent GETs (the listener reconnects after
        // each clean close) return 405 so the listener gives up
        // gracefully after proving the reconnect path was entered.
        if request.method == "GET" {
            let n = get_count_clone.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                return HttpResponseSpec::sse(
                    "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n"
                        .to_string(),
                );
            }
            return HttpResponseSpec::text(405, "no more streams");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({})),
            "tools/list" => {
                let n = tools_list_count_clone.fetch_add(1, Ordering::SeqCst) + 1;
                // Issue a different tool catalog on the refresh so we
                // can also verify the rebuild_snapshot path (R8 #3)
                // surfaces the new tool through the manager — not
                // just that refresh_server ran.
                let tool_name = if n == 1 { "echo" } else { "echov2" };
                HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request.body["id"].clone(),
                    "result": {
                        "tools": [{
                            "name": tool_name,
                            "inputSchema": {"type": "object", "properties": {}}
                        }]
                    }
                }))
            }
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_listener", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();

    // Discovery ran tools/list once during connect. Initial catalog
    // is "echo".
    let registry = manager.registry();
    let initial_ids: Vec<String> = registry.ids().into_iter().collect();
    assert!(
        initial_ids.iter().any(|id| id.contains("echo")),
        "initial registry must contain echo. ids = {initial_ids:?}"
    );
    assert!(
        !initial_ids.iter().any(|id| id.contains("echov2")),
        "initial registry must NOT contain echov2. ids = {initial_ids:?}"
    );

    // Wait for: (a) the background GET to be issued; (b) the
    // list_changed event to propagate; (c) the watcher to call
    // tools/list a second time; (d) rebuild_snapshot to publish.
    let observed = wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
        let manager_ids: Vec<String> = manager.registry().ids().into_iter().collect();
        manager_ids.iter().any(|id| id.contains("echov2"))
    })
    .await;

    server.abort();

    assert!(
        observed,
        "expected echov2 to appear in registry after list_changed-driven refresh. \
         tools/list count = {}, GET count = {}",
        tools_list_count.load(Ordering::SeqCst),
        get_count.load(Ordering::SeqCst)
    );

    // Sanity: the listener actually fired and was the trigger.
    assert!(
        get_count.load(Ordering::SeqCst) >= 1,
        "GET listening stream must have been opened at least once"
    );
    assert!(
        tools_list_count.load(Ordering::SeqCst) >= 2,
        "tools/list must have run at least twice (discovery + refresh)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_listening_stream_backs_off_on_non_sse_success() {
    let get_count = Arc::new(AtomicUsize::new(0));
    let get_count_for_handler = Arc::clone(&get_count);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            get_count_for_handler.fetch_add(1, Ordering::SeqCst);
            return HttpResponseSpec::json(json!({"status": "not-sse"}));
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"tools": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "echo",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_listener_non_sse", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();

    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            get_count.load(Ordering::SeqCst) >= 1
        })
        .await,
        "background GET should be attempted"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    manager.close_all().await.expect("close manager");
    server.abort();

    assert_eq!(
        get_count.load(Ordering::SeqCst),
        1,
        "non-SSE 200 response should back off instead of hot-looping"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_listening_stream_respects_retry_after_clean_close() {
    let get_count = Arc::new(AtomicUsize::new(0));
    let get_count_for_handler = Arc::clone(&get_count);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            get_count_for_handler.fetch_add(1, Ordering::SeqCst);
            return HttpResponseSpec::sse("retry: 500\n\n");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"tools": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "echo",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_listener_retry", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();

    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            get_count.load(Ordering::SeqCst) >= 1
        })
        .await,
        "background GET should be attempted"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    manager.close_all().await.expect("close manager");
    server.abort();

    assert_eq!(
        get_count.load(Ordering::SeqCst),
        1,
        "retry: 500 should prevent an immediate clean-close reconnect"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_listening_stream_404_pauses_until_reinitialize() {
    let get_count = Arc::new(AtomicUsize::new(0));
    let get_count_for_handler = Arc::clone(&get_count);
    let expire_get = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let expire_get_for_handler = Arc::clone(&expire_get);
    let get_headers = Arc::new(Mutex::new(
        Vec::<std::collections::HashMap<String, String>>::new(),
    ));
    let get_headers_for_handler = Arc::clone(&get_headers);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            get_count_for_handler.fetch_add(1, Ordering::SeqCst);
            get_headers_for_handler
                .lock()
                .unwrap()
                .push(request.headers.clone());
            if expire_get_for_handler.load(Ordering::SeqCst) {
                return HttpResponseSpec::text(404, "session expired");
            }
            return HttpResponseSpec::sse("retry: 1000\n\n");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"tools": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "echo",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_listener_404_pause", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();

    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            get_count.load(Ordering::SeqCst) >= 1
        })
        .await,
        "initial background GET should be attempted"
    );
    expire_get.store(true, Ordering::SeqCst);
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            get_count.load(Ordering::SeqCst) >= 2
        })
        .await,
        "listener should observe the session-expired 404"
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    manager.close_all().await.expect("close manager");
    server.abort();

    let headers = get_headers.lock().unwrap().clone();
    assert_eq!(
        headers.len(),
        2,
        "listener must not keep issuing GETs while protocol_version is cleared"
    );
    assert!(
        headers
            .iter()
            .all(|headers| headers.contains_key("mcp-protocol-version")),
        "every GET issued by an initialized listener must carry MCP-Protocol-Version: {headers:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_listening_stream_drops_last_event_id_after_reinitialize() {
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let initialize_count_for_handler = Arc::clone(&initialize_count);
    let tools_call_count = Arc::new(AtomicUsize::new(0));
    let tools_call_count_for_handler = Arc::clone(&tools_call_count);
    let get_count = Arc::new(AtomicUsize::new(0));
    let get_count_for_handler = Arc::clone(&get_count);
    let get_headers = Arc::new(Mutex::new(
        Vec::<std::collections::HashMap<String, String>>::new(),
    ));
    let get_headers_for_handler = Arc::clone(&get_headers);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            let n = get_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
            get_headers_for_handler
                .lock()
                .unwrap()
                .push(request.headers.clone());
            if n == 1 {
                return HttpResponseSpec::sse("id: old-1\nretry: 300\n\n");
            }
            return HttpResponseSpec::text(405, "listener stopped");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => {
                let n = initialize_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                HttpResponseSpec::json_with_headers(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "test-server", "version": "1.0.0"}
                        }
                    }),
                    vec![("MCP-Session-Id", format!("session-{n}"))],
                )
            }
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "listener_cursor",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => {
                let n = tools_call_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    HttpResponseSpec::text(404, "session gone")
                } else {
                    HttpResponseSpec::json(json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "content": [{"type": "text", "text": "retry ok"}]
                        }
                    }))
                }
            }
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_listener_cursor", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            get_count.load(Ordering::SeqCst) >= 1
        })
        .await,
        "listener should capture an event id on session-1"
    );

    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__listener_cursor"))
        .expect("discover listener_cursor tool");
    let tool = registry.get(&tool_id).expect("registry tool");
    let result = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect("tools/call should reinitialize after 404");
    assert_eq!(result.result.data, json!("retry ok"));

    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            get_count.load(Ordering::SeqCst) >= 2
        })
        .await,
        "listener should reconnect after reinitialize"
    );

    manager.close_all().await.expect("close manager");
    server.abort();

    assert_eq!(initialize_count.load(Ordering::SeqCst), 2);
    assert_eq!(tools_call_count.load(Ordering::SeqCst), 2);
    let headers = get_headers.lock().unwrap().clone();
    assert!(
        headers.len() >= 2,
        "expected at least two listener GETs, got {headers:?}"
    );
    assert_eq!(
        headers[1].get("mcp-session-id"),
        Some(&"session-2".to_string())
    );
    assert!(
        !headers[1].contains_key("last-event-id"),
        "new session must not inherit old Last-Event-ID: {:?}",
        headers[1]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_listening_stream_request_is_not_answered_on_new_session() {
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let initialize_count_for_handler = Arc::clone(&initialize_count);
    let tools_call_count = Arc::new(AtomicUsize::new(0));
    let tools_call_count_for_handler = Arc::clone(&tools_call_count);
    let get_count = Arc::new(AtomicUsize::new(0));
    let get_count_for_handler = Arc::clone(&get_count);
    let release_stale_stream = Arc::new(AtomicBool::new(false));
    let release_stale_stream_for_handler = Arc::clone(&release_stale_stream);
    let stale_stream_sent = Arc::new(AtomicBool::new(false));
    let stale_stream_sent_for_handler = Arc::clone(&stale_stream_sent);
    let listener_response_posts = Arc::new(Mutex::new(Vec::<HttpRequestSpec>::new()));
    let listener_response_posts_for_handler = Arc::clone(&listener_response_posts);

    let main_handler = Arc::new(move |request: HttpRequestSpec| {
        if request.method == "GET" {
            let n = get_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                while !release_stale_stream_for_handler.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                stale_stream_sent_for_handler.store(true, Ordering::SeqCst);
                return HttpResponseSpec::sse(format!(
                    "data: {}\n\n",
                    json!({
                        "jsonrpc": "2.0",
                        "id": 77,
                        "method": "roots/list"
                    })
                ));
            }
            return HttpResponseSpec::text(405, "listener stopped");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => {
                let n = initialize_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                HttpResponseSpec::json_with_headers(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "test-server", "version": "1.0.0"}
                        }
                    }),
                    vec![("MCP-Session-Id", format!("session-{n}"))],
                )
            }
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "stale_listener_request",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => {
                let n = tools_call_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    HttpResponseSpec::text(404, "session gone")
                } else {
                    HttpResponseSpec::json(json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "content": [{"type": "text", "text": "retry ok"}]
                        }
                    }))
                }
            }
            other => panic!("unexpected method: {other}"),
        }
    });
    let response_handler = Arc::new(move |request: HttpRequestSpec| {
        listener_response_posts_for_handler
            .lock()
            .unwrap()
            .push(request);
        HttpResponseSpec::accepted()
    });
    let (endpoint, server) =
        spawn_http_server_with_response_handler(main_handler, Some(response_handler)).await;

    let cfg = McpServerConnectionConfig::http("http_stale_listener_request", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            get_count.load(Ordering::SeqCst) >= 1
        })
        .await,
        "session-1 listener GET should be in flight"
    );

    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__stale_listener_request"))
        .expect("discover stale_listener_request tool");
    let tool = registry.get(&tool_id).expect("registry tool");
    let result = tool
        .execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect("tools/call should reinitialize after 404");
    assert_eq!(result.result.data, json!("retry ok"));
    assert_eq!(initialize_count.load(Ordering::SeqCst), 2);

    release_stale_stream.store(true, Ordering::SeqCst);
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            stale_stream_sent.load(Ordering::SeqCst)
        })
        .await,
        "stale session-1 stream should send its server request"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    manager.close_all().await.expect("close manager");
    server.abort();

    assert!(
        listener_response_posts.lock().unwrap().is_empty(),
        "client must not answer stale stream requests using the new session: {:?}",
        listener_response_posts
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.headers.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_listener_404_after_initialize_forces_next_call_reinitialize() {
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let initialize_count_for_handler = Arc::clone(&initialize_count);
    let get_count = Arc::new(AtomicUsize::new(0));
    let get_count_for_handler = Arc::clone(&get_count);
    let release_get_404 = Arc::new(AtomicBool::new(false));
    let release_get_404_for_handler = Arc::clone(&release_get_404);
    let get_404_count = Arc::new(AtomicUsize::new(0));
    let get_404_count_for_handler = Arc::clone(&get_404_count);
    let tool_call_sessions = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let tool_call_sessions_for_handler = Arc::clone(&tool_call_sessions);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            let n = get_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                while !release_get_404_for_handler.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                get_404_count_for_handler.fetch_add(1, Ordering::SeqCst);
                return HttpResponseSpec::text(404, "session expired");
            }
            return HttpResponseSpec::text(405, "background listener disabled");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => {
                let n = initialize_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                HttpResponseSpec::json_with_headers(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "test-server", "version": "1.0.0"}
                        }
                    }),
                    vec![("MCP-Session-Id", format!("session-{n}"))],
                )
            }
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "echo",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => {
                if request.headers.get("mcp-session-id").map(String::as_str) == Some("session-1") {
                    return HttpResponseSpec::text(404, "session expired");
                }
                tool_call_sessions_for_handler
                    .lock()
                    .unwrap()
                    .push(request.headers.get("mcp-session-id").cloned());
                HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request.body["id"].clone(),
                    "result": {
                        "content": [{"type": "text", "text": "ok"}]
                    }
                }))
            }
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_listener_init_404", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            get_count.load(Ordering::SeqCst) >= 1
        })
        .await,
        "listener GET should be issued after initialize capabilities are committed"
    );
    release_get_404.store(true, Ordering::SeqCst);
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            get_404_count.load(Ordering::SeqCst) >= 1
        })
        .await,
        "listener should observe immediate session-expired 404"
    );

    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__echo"))
        .expect("discover echo tool");
    let tool = registry.get(&tool_id).expect("registry tool");
    tool.execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect("next call should reinitialize after listener 404");

    manager.close_all().await.expect("close manager");
    server.abort();

    assert_eq!(
        initialize_count.load(Ordering::SeqCst),
        2,
        "listener 404 after initialize must clear capabilities so the next call reinitializes"
    );
    assert_eq!(
        tool_call_sessions.lock().unwrap().as_slice(),
        &[Some("session-2".to_string())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_listener_response_404_resets_session_before_next_call() {
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let initialize_count_for_handler = Arc::clone(&initialize_count);
    let response_404_count = Arc::new(AtomicUsize::new(0));
    let response_404_count_for_handler = Arc::clone(&response_404_count);
    let tool_call_sessions = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let tool_call_sessions_for_handler = Arc::clone(&tool_call_sessions);

    let main_handler = Arc::new(move |request: HttpRequestSpec| {
        if request.method == "GET" {
            return HttpResponseSpec::sse(format!(
                "data: {}\n\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "roots/list"
                })
            ));
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => {
                let n = initialize_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                HttpResponseSpec::json_with_headers(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "test-server", "version": "1.0.0"}
                        }
                    }),
                    vec![("MCP-Session-Id", format!("session-{n}"))],
                )
            }
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "echo",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => {
                tool_call_sessions_for_handler
                    .lock()
                    .unwrap()
                    .push(request.headers.get("mcp-session-id").cloned());
                HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request.body["id"].clone(),
                    "result": {
                        "content": [{"type": "text", "text": "ok"}]
                    }
                }))
            }
            other => panic!("unexpected method: {other}"),
        }
    });
    let response_handler = Arc::new(move |_request: HttpRequestSpec| {
        response_404_count_for_handler.fetch_add(1, Ordering::SeqCst);
        HttpResponseSpec::text(404, "session expired")
    });
    let (endpoint, server) =
        spawn_http_server_with_response_handler(main_handler, Some(response_handler)).await;

    let cfg = McpServerConnectionConfig::http("http_listener_response_404", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let initial_session_generation = manager
        .server_status_snapshot("http_listener_response_404")
        .await
        .expect("initial status")
        .session_generation;
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            response_404_count.load(Ordering::SeqCst) >= 1
        })
        .await,
        "listener should POST a response and observe the 404"
    );
    let start = Instant::now();
    let mut reset_observed = initialize_count.load(Ordering::SeqCst) >= 2;
    while start.elapsed() <= Duration::from_secs(2) {
        let status = manager
            .server_status_snapshot("http_listener_response_404")
            .await
            .expect("status after listener response 404");
        if status.session_generation > initial_session_generation
            || initialize_count.load(Ordering::SeqCst) >= 2
        {
            reset_observed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        reset_observed,
        "listener response 404 should reset HTTP session generation"
    );

    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__echo"))
        .expect("discover echo tool");
    let tool = registry.get(&tool_id).expect("registry tool");
    tool.execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect("call should reinitialize after listener response 404");

    manager.close_all().await.expect("close manager");
    server.abort();

    assert!(
        initialize_count.load(Ordering::SeqCst) >= 2,
        "listener response 404 should force a fresh initialize before the next call"
    );
    assert_eq!(
        tool_call_sessions.lock().unwrap().as_slice(),
        &[Some("session-2".to_string())],
        "next tool call should use the reinitialized session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_listener_get_404_does_not_clear_fresh_session() {
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let initialize_count_for_handler = Arc::clone(&initialize_count);
    let get_count = Arc::new(AtomicUsize::new(0));
    let get_count_for_handler = Arc::clone(&get_count);
    let release_stale_get = Arc::new(AtomicBool::new(false));
    let release_stale_get_for_handler = Arc::clone(&release_stale_get);
    let tools_call_count = Arc::new(AtomicUsize::new(0));
    let tools_call_count_for_handler = Arc::clone(&tools_call_count);
    let tool_call_sessions = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let tool_call_sessions_for_handler = Arc::clone(&tool_call_sessions);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            let n = get_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                while !release_stale_get_for_handler.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                return HttpResponseSpec::text(404, "old session expired");
            }
            return HttpResponseSpec::text(405, "background listener disabled");
        }
        if request.method == "DELETE" {
            return HttpResponseSpec::text(200, "");
        }

        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => {
                let n = initialize_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                HttpResponseSpec::json_with_headers(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "test-server", "version": "1.0.0"}
                        }
                    }),
                    vec![("MCP-Session-Id", format!("session-{n}"))],
                )
            }
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "echo",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
            "tools/call" => {
                let n = tools_call_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                tool_call_sessions_for_handler
                    .lock()
                    .unwrap()
                    .push(request.headers.get("mcp-session-id").cloned());
                if n == 1 {
                    HttpResponseSpec::text(404, "session expired")
                } else {
                    HttpResponseSpec::json(json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "content": [{"type": "text", "text": format!("call #{n} ok")}]
                        }
                    }))
                }
            }
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_stale_listener_404", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            get_count.load(Ordering::SeqCst) >= 1
        })
        .await,
        "background GET should be in flight under session-1"
    );

    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__echo"))
        .expect("discover echo tool");
    let tool = registry.get(&tool_id).expect("registry tool");
    tool.execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect("first call should reinitialize after POST 404");
    assert_eq!(
        initialize_count.load(Ordering::SeqCst),
        2,
        "first call should install session-2"
    );

    release_stale_get.store(true, Ordering::SeqCst);
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            get_count.load(Ordering::SeqCst) >= 2
        })
        .await,
        "listener should continue after the stale 404 without clearing session-2"
    );

    tool.execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect("second call should keep using session-2");

    manager.close_all().await.expect("close manager");
    server.abort();

    assert_eq!(
        initialize_count.load(Ordering::SeqCst),
        2,
        "stale listener 404 must not force a third initialize"
    );
    assert_eq!(
        tool_call_sessions.lock().unwrap().as_slice(),
        &[
            Some("session-1".to_string()),
            Some("session-2".to_string()),
            Some("session-2".to_string())
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn factory_only_sampling_rejects_background_get_request() {
    struct OkSamplingHandler;

    #[async_trait]
    impl SamplingHandler for OkSamplingHandler {
        async fn handle_create_message(
            &self,
            _params: CreateMessageParams,
        ) -> Result<CreateMessageResult, McpTransportError> {
            use mcp::{Role, SamplingContent};
            Ok(CreateMessageResult {
                role: Role::Assistant,
                content: vec![SamplingContent::Text {
                    text: "ok".to_string(),
                    annotations: None,
                    meta: None,
                }],
                model: "test-model".to_string(),
                stop_reason: None,
                meta: None,
            })
        }
    }

    struct AlwaysSamplingFactory {
        handler: Arc<dyn SamplingHandler>,
    }

    #[async_trait]
    impl SamplingHandlerFactory for AlwaysSamplingFactory {
        async fn for_agent(&self, _agent_spec: &AgentSpec) -> Option<Arc<dyn SamplingHandler>> {
            Some(Arc::clone(&self.handler))
        }
    }

    let get_count = Arc::new(AtomicUsize::new(0));
    let get_count_for_handler = Arc::clone(&get_count);
    let initialize_capabilities = Arc::new(std::sync::Mutex::new(None::<Value>));
    let initialize_capabilities_for_handler = Arc::clone(&initialize_capabilities);
    let sampling_responses = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let sampling_responses_for_observer = Arc::clone(&sampling_responses);

    let (endpoint, server) = spawn_http_server_with_response_observer(
        Arc::new(move |request| {
            if request.method == "GET" {
                let n = get_count_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    return HttpResponseSpec::sse(format!(
                        "data: {}\n\n",
                        json!({
                            "jsonrpc": "2.0",
                            "id": 99,
                            "method": "sampling/createMessage",
                            "params": {
                                "messages": [],
                                "maxTokens": 1
                            }
                        })
                    ));
                }
                return HttpResponseSpec::text(405, "listening stream closed");
            }
            if request.method == "DELETE" {
                return HttpResponseSpec::text(200, "");
            }

            match request.body["method"].as_str().unwrap_or_default() {
                "notifications/initialized" => HttpResponseSpec::accepted(),
                "initialize" => {
                    *initialize_capabilities_for_handler.lock().unwrap() =
                        Some(request.body["params"]["capabilities"].clone());
                    initialize_response(&request, json!({"tools": {}}))
                }
                "tools/list" => HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request.body["id"].clone(),
                    "result": {
                        "tools": [{
                            "name": "echo",
                            "inputSchema": {"type": "object", "properties": {}}
                        }]
                    }
                })),
                other => panic!("unexpected method: {other}"),
            }
        }),
        Some(Arc::new(move |request| {
            sampling_responses_for_observer
                .lock()
                .unwrap()
                .push(request.body);
        })),
    )
    .await;

    let handler = Arc::new(OkSamplingHandler) as Arc<dyn SamplingHandler>;
    let factory = Arc::new(AlwaysSamplingFactory { handler }) as Arc<dyn SamplingHandlerFactory>;
    let cfg = McpServerConnectionConfig::http("http_factory_only_sampling", endpoint);
    let manager = McpToolRegistryManager::connect_with_sampling_factory([cfg], None, Some(factory))
        .await
        .expect("connect factory-only sampling registry");

    assert!(
        !initialize_capabilities
            .lock()
            .unwrap()
            .as_ref()
            .expect("initialize capabilities captured")
            .as_object()
            .expect("capabilities object")
            .contains_key("sampling"),
        "factory-only sampling must not advertise global sampling capability"
    );
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            !sampling_responses.lock().unwrap().is_empty()
        })
        .await,
        "background GET sampling response should be posted"
    );
    manager.close_all().await.expect("close manager");
    server.abort();

    let responses = sampling_responses.lock().unwrap();
    let response = responses.first().expect("sampling error response");
    assert_eq!(response["id"], json!(99));
    assert_eq!(response["error"]["code"], json!(-32601));
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Sampling not supported")
    );
}

/// R8 #5 / R10 #4 regression: HTTP `call_tool` retries once on 404
/// "MCP session expired". The first `tools/call` after a session
/// invalidation gets a 404; the transport must `reset_session`,
/// re-initialize, allocate a fresh request id, and re-send. Without
/// the retry, every `tools/call` after a server-side session purge
/// fails — even though the spec explicitly allows reconnection.
#[tokio::test]
async fn http_call_tool_retries_once_on_session_expired() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tools_call_count = Arc::new(AtomicUsize::new(0));
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let tools_call_count_clone = Arc::clone(&tools_call_count);
    let initialize_count_clone = Arc::clone(&initialize_count);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            return HttpResponseSpec::text(405, "no listening stream");
        }
        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => {
                // Issue a fresh session id on each initialize so the
                // test can detect that a NEW session was established
                // (the second one) rather than the original being reused.
                let n = initialize_count_clone.fetch_add(1, Ordering::SeqCst) + 1;
                HttpResponseSpec::json_with_headers(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                            "capabilities": {},
                            "serverInfo": {
                                "name": "test-server",
                                "version": "1.0.0"
                            }
                        }
                    }),
                    vec![("MCP-Session-Id", format!("session-{n}"))],
                )
            }
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{"name": "echo", "inputSchema": {"type": "object", "properties": {}}}]
                }
            })),
            "tools/call" => {
                let n = tools_call_count_clone.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    // First call: simulate session expiry. The 404
                    // path in `post_message` maps this to
                    // `ProtocolError("MCP session expired")` when the
                    // request carried a session id (which it did,
                    // because initialize set one). The retry loop in
                    // HTTP `call_tool` then resets + re-initializes +
                    // retries with a new id.
                    HttpResponseSpec::text(404, "session gone")
                } else {
                    HttpResponseSpec::json(json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "content": [{"type": "text", "text": format!("call #{n} ok")}]
                        }
                    }))
                }
            }
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_session_retry", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry.ids().into_iter().next().unwrap();
    let tool = registry.get(&tool_id).unwrap();
    let status_before = manager
        .server_status_snapshot("http_session_retry")
        .await
        .expect("initial server status");
    let initial_session_generation = status_before.session_generation;
    let initial_last_init_at = status_before
        .last_init_at
        .expect("initial last_init_at should be set");
    tokio::time::sleep(Duration::from_millis(5)).await;

    let ctx = ToolCallContext::test_default();
    let result = tool
        .execute(json!({"message":"x"}), &ctx)
        .await
        .expect("call_tool should succeed via session-expired retry");
    let status_after = manager
        .server_status_snapshot("http_session_retry")
        .await
        .expect("status after silent reinitialize");

    server.abort();

    // The second tools/call (retry) succeeded — the result text
    // contains the "call #2 ok" from our handler.
    let text = result.result.data.to_string();
    assert!(
        text.contains("call #2 ok"),
        "expected retry response in result, got: {text}"
    );
    assert!(
        status_after.session_generation > initial_session_generation,
        "silent reinitialize must advance session_generation; before={initial_session_generation:?}, after={:?}",
        status_after.session_generation
    );
    assert!(
        status_after.last_init_at.expect("updated last_init_at") > initial_last_init_at,
        "silent reinitialize must update live last_init_at; before={initial_last_init_at:?}, after={:?}",
        status_after.last_init_at
    );

    // Verify the wire trace: initialize ran twice (original + post-404
    // re-initialize), and tools/call ran twice (first 404'd, second
    // succeeded). One retry, not infinite.
    assert_eq!(
        initialize_count.load(Ordering::SeqCst),
        2,
        "initialize must run twice: original + retry after session reset"
    );
    assert_eq!(
        tools_call_count.load(Ordering::SeqCst),
        2,
        "tools/call must run twice: first 404, retry succeeds. No infinite retry."
    );
}

#[tokio::test]
async fn manager_uses_live_capabilities_after_http_silent_reinitialize() {
    let tools_call_count = Arc::new(AtomicUsize::new(0));
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let subscribe_count = Arc::new(AtomicUsize::new(0));
    let tools_call_count_clone = Arc::clone(&tools_call_count);
    let initialize_count_clone = Arc::clone(&initialize_count);
    let subscribe_count_clone = Arc::clone(&subscribe_count);

    let (endpoint, server) = spawn_http_server(Arc::new(move |request| {
        if request.method == "GET" {
            return HttpResponseSpec::text(405, "no listening stream");
        }
        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => {
                let n = initialize_count_clone.fetch_add(1, Ordering::SeqCst) + 1;
                let can_subscribe = n >= 2;
                HttpResponseSpec::json_with_headers(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                            "capabilities": {
                                "resources": {"subscribe": can_subscribe}
                            },
                            "serverInfo": {
                                "name": "test-server",
                                "version": "1.0.0"
                            }
                        }
                    }),
                    vec![("MCP-Session-Id", format!("session-{n}"))],
                )
            }
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{"name": "echo", "inputSchema": {"type": "object", "properties": {}}}]
                }
            })),
            "tools/call" => {
                let n = tools_call_count_clone.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    HttpResponseSpec::text(404, "session gone")
                } else {
                    HttpResponseSpec::json(json!({
                        "jsonrpc": "2.0",
                        "id": request.body["id"].clone(),
                        "result": {
                            "content": [{"type": "text", "text": "retry ok"}]
                        }
                    }))
                }
            }
            "resources/subscribe" => {
                subscribe_count_clone.fetch_add(1, Ordering::SeqCst);
                HttpResponseSpec::json(json!({
                    "jsonrpc": "2.0",
                    "id": request.body["id"].clone(),
                    "result": {}
                }))
            }
            other => panic!("unexpected method: {other}"),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_live_caps_retry", endpoint);
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry.ids().into_iter().next().unwrap();
    let tool = registry.get(&tool_id).unwrap();
    tool.execute(json!({}), &ToolCallContext::test_default())
        .await
        .expect("call_tool should reinitialize after session expiry");

    manager
        .subscribe_resource("http_live_caps_retry", "file:///tmp/item")
        .await
        .expect("manager should gate subscribe with live post-reinitialize capabilities");

    server.abort();

    assert_eq!(initialize_count.load(Ordering::SeqCst), 2);
    assert_eq!(tools_call_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        subscribe_count.load(Ordering::SeqCst),
        1,
        "subscribe_resource should be sent after live capabilities refresh"
    );
}

#[tokio::test]
async fn http_call_tool_with_is_error_result_returns_tool_error_result() {
    let (endpoint, server) = spawn_http_server(Arc::new(|request| {
        if request.method == "GET" {
            return HttpResponseSpec::text(405, "no listening stream");
        }
        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{"name": "echo", "inputSchema": {"type": "object", "properties": {}}}]
                }
            })),
            "tools/call" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
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
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry.ids().into_iter().next().unwrap();
    let tool = registry.get(&tool_id).unwrap();

    let ctx = ToolCallContext::test_default();
    let output = tool
        .execute(json!({"message":"x"}), &ctx)
        .await
        .expect("MCP tool execution errors are successful JSON-RPC results");
    server.abort();
    assert!(output.result.is_error());
    assert_eq!(output.result.data, Value::String("tool failed".to_string()));
    assert_eq!(output.result.message.as_deref(), Some("tool failed"));
    assert_eq!(
        output.result.metadata["mcp.result.isError"],
        Value::Bool(true)
    );
    assert_eq!(
        output.result.metadata["mcp.result.content"],
        json!([{"type": "text", "text": "tool failed"}])
    );
}

#[tokio::test]
async fn http_call_tool_preserves_structured_content() {
    let (endpoint, server) = spawn_http_server(Arc::new(|request| {
        if request.method == "GET" {
            return HttpResponseSpec::text(405, "no listening stream");
        }
        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {
                    "tools": [{"name": "sum", "inputSchema": {"type": "object", "properties": {}}}]
                }
            })),
            "tools/call" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
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
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let tool_id = registry.ids().into_iter().next().unwrap();
    let tool = registry.get(&tool_id).unwrap();

    let ctx = ToolCallContext::test_default();
    let result = tool
        .execute(json!({"values":[1,2]}), &ctx)
        .await
        .expect("structured tool result");
    server.abort();

    assert_eq!(
        result.result.metadata["mcp.result.structuredContent"]["sum"],
        json!(3)
    );
}

#[tokio::test]
async fn http_list_prompts_parses_prompt_definitions() {
    let (endpoint, server) = spawn_http_server(Arc::new(|request| {
        if request.method == "GET" {
            return HttpResponseSpec::text(405, "no listening stream");
        }
        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"prompts": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {"tools": []}
            })),
            "prompts/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
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
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let prompts = manager.list_prompts().await.expect("prompt list");
    server.abort();

    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].prompt.name, "review");
    assert_eq!(prompts[0].prompt.arguments.len(), 1);
    assert!(prompts[0].prompt.arguments[0].required);
}

#[tokio::test]
async fn http_list_resources_parses_resource_definitions() {
    let (endpoint, server) = spawn_http_server(Arc::new(|request| {
        if request.method == "GET" {
            return HttpResponseSpec::text(405, "no listening stream");
        }
        match request.body["method"].as_str().unwrap_or_default() {
            "notifications/initialized" => HttpResponseSpec::accepted(),
            "initialize" => initialize_response(&request, json!({"resources": {}})),
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
                "result": {"tools": []}
            })),
            "resources/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request.body["id"].clone(),
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
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let resources = manager.list_resources().await.expect("resource list");
    server.abort();

    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].resource.uri, "file://guide.md");
    assert_eq!(
        resources[0].resource.mime_type.as_deref(),
        Some("text/markdown")
    );
    assert_eq!(resources[0].resource.size, Some(42));
}

// ── Plugin test ──

#[tokio::test]
async fn mcp_plugin_descriptor() {
    use awaken_ext_mcp::McpPlugin;
    use awaken_runtime::Plugin;

    let transport = Arc::new(FakeTransport::new(vec![])) as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let plugin = McpPlugin::new(manager.registry());
    let desc = plugin.descriptor();
    assert_eq!(desc.name, "mcp");
}

// ── Registry API tests ──

#[tokio::test]
async fn registry_len_is_empty_ids() {
    let transport = Arc::new(FakeTransport::new(vec![
        McpToolDefinition::new("a"),
        McpToolDefinition::new("b"),
    ])) as Arc<dyn McpToolTransport>;

    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();

    assert_eq!(reg.len(), 2);
    assert!(!reg.is_empty());
    assert_eq!(reg.ids().len(), 2);
}

#[tokio::test]
async fn registry_get_returns_none_for_unknown() {
    let transport = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]))
        as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();

    assert!(reg.get("nonexistent").is_none());
}

#[tokio::test]
async fn registry_snapshot_returns_all_tools() {
    let transport = Arc::new(FakeTransport::new(vec![
        McpToolDefinition::new("a"),
        McpToolDefinition::new("b"),
    ])) as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();

    let snap = reg.snapshot();
    assert_eq!(snap.len(), 2);
}

#[tokio::test]
async fn manager_servers_returns_connected_servers() {
    let transport = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]))
        as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();

    let servers = manager.servers();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].0, "s1");
    assert_eq!(servers[0].1, TransportTypeId::Stdio);
}

#[tokio::test]
async fn manager_debug_impl_works() {
    let transport = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]))
        as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let debug = format!("{:?}", manager);
    assert!(debug.contains("McpToolRegistryManager"));
    assert!(debug.contains("servers: 1"));
}

#[tokio::test]
async fn registry_debug_impl_works() {
    let transport = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]))
        as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();
    let debug = format!("{:?}", reg);
    assert!(debug.contains("McpToolRegistry"));
}

// ── Tool descriptor tests ──

#[tokio::test]
async fn tool_descriptor_has_parameters_from_mcp_schema() {
    let def = McpToolDefinition::new("calc")
        .with_title("Calculator")
        .with_description("Math operations");

    let transport = Arc::new(FakeTransport::new(vec![def])) as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();
    let tool_id = reg.ids().into_iter().next().unwrap();
    let tool = reg.get(&tool_id).unwrap();
    let desc = tool.descriptor();

    assert_eq!(desc.name, "Calculator");
    assert!(desc.description.contains("Math operations"));
    assert_eq!(
        desc.metadata.get("mcp.server").and_then(|v| v.as_str()),
        Some("s1")
    );
}

#[tokio::test]
async fn tool_with_category_preserves_group() {
    let mut def = McpToolDefinition::new("echo");
    def.group = Some("utilities".to_string());

    let transport = Arc::new(FakeTransport::new(vec![def])) as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();
    let tool_id = reg.ids().into_iter().next().unwrap();
    let tool = reg.get(&tool_id).unwrap();
    let desc = tool.descriptor();

    assert_eq!(desc.category.as_deref(), Some("utilities"));
}

// ── MCP result data extraction tests ──

#[tokio::test]
async fn plain_text_result_becomes_string_data() {
    let transport = Arc::new(FakeTransport::new(vec![McpToolDefinition::new("echo")]))
        as Arc<dyn McpToolTransport>;
    let manager = McpToolRegistryManager::from_transports([(cfg("s1"), transport)])
        .await
        .unwrap();
    let reg = manager.registry();
    let tool_id = reg.ids().into_iter().next().unwrap();
    let tool = reg.get(&tool_id).unwrap();

    let ctx = ToolCallContext::test_default();
    let result = tool.execute(json!({}), &ctx).await.unwrap();
    // Plain text "ok" is stored directly as a string in data (no wrapping)
    assert_eq!(result.result.data, json!("ok"));
    assert!(result.result.metadata["mcp.server"].is_string());
}

// ── Error variant tests ──

#[test]
fn mcp_error_display_strings() {
    assert_eq!(
        McpError::EmptyServerName.to_string(),
        "server name must be non-empty"
    );
    assert_eq!(
        McpError::DuplicateServerName("s1".into()).to_string(),
        "duplicate server name: s1"
    );
    assert_eq!(
        McpError::UnknownServer("s1".into()).to_string(),
        "unknown mcp server: s1"
    );
    assert_eq!(
        McpError::InvalidToolIdComponent("bad".into()).to_string(),
        "invalid tool id component after sanitization: bad"
    );
    assert_eq!(
        McpError::ToolIdConflict("id".into()).to_string(),
        "tool id already registered: id"
    );
    assert_eq!(
        McpError::Transport("err".into()).to_string(),
        "mcp transport error: err"
    );
    assert_eq!(
        McpError::InvalidRefreshInterval.to_string(),
        "periodic refresh interval must be > 0"
    );
    assert_eq!(
        McpError::PeriodicRefreshAlreadyRunning.to_string(),
        "periodic refresh loop is already running"
    );
    assert_eq!(
        McpError::RuntimeUnavailable.to_string(),
        "tokio runtime is required to start periodic refresh"
    );
}

#[test]
fn mcp_error_from_transport_error() {
    let transport_err = McpTransportError::TransportError("conn failed".into());
    let mcp_err: McpError = transport_err.into();
    assert!(matches!(mcp_err, McpError::Transport(_)));
    assert!(mcp_err.to_string().contains("conn failed"));
}
