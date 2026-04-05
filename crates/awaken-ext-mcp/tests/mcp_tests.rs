//! Integration tests for the awaken-ext-mcp crate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use awaken_contract::contract::tool::ToolCallContext;
use awaken_ext_mcp::{
    McpError, McpProgressUpdate, McpPromptArgument, McpPromptDefinition, McpPromptMessage,
    McpPromptResult, McpResourceDefinition, McpServerConnectionConfig, McpToolRegistryManager,
    McpToolTransport, SamplingHandler,
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
        _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
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
        progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
    ) -> Result<CallToolResult, McpTransportError> {
        if let Some(progress_tx) = progress_tx {
            let _ = progress_tx.send(McpProgressUpdate {
                progress: 3.0,
                total: Some(10.0),
                message: Some("working".to_string()),
            });
            let _ = progress_tx.send(McpProgressUpdate {
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
        _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
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
        _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
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
}

impl FakeUiTransport {
    fn new(tools: Vec<McpToolDefinition>) -> Self {
        Self {
            tools,
            resources: HashMap::new(),
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
        _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
    ) -> Result<CallToolResult, McpTransportError> {
        Ok(ok_text_result("ok"))
    }

    fn transport_type(&self) -> TransportTypeId {
        TransportTypeId::Stdio
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, McpTransportError> {
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
    headers: std::collections::HashMap<String, String>,
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
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
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

    let headers = std::str::from_utf8(&buf[..header_end_pos]).ok()?;
    let body = serde_json::from_slice(&buf[header_end_pos..header_end_pos + body_len]).ok()?;
    Some(HttpRequestSpec {
        headers: parse_headers(headers),
        body,
    })
}

async fn spawn_http_server(
    handler: Arc<dyn Fn(HttpRequestSpec) -> HttpResponseSpec + Send + Sync>,
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
                let Some(request) = read_http_request(&mut stream).await else {
                    return;
                };
                let response = if request.body["method"].is_null()
                    && (request.body.get("result").is_some() || request.body.get("error").is_some())
                {
                    HttpResponseSpec::accepted()
                } else {
                    handler(request)
                };
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
            _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
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
            _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
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

#[tokio::test]
async fn http_call_tool_with_is_error_result_returns_server_error() {
    let (endpoint, server) = spawn_http_server(Arc::new(|request| {
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
    let err = tool
        .execute(json!({"message":"x"}), &ctx)
        .await
        .expect_err("should fail");
    server.abort();
    assert!(matches!(
        err,
        awaken_contract::contract::tool::ToolError::ExecutionFailed(_)
    ));
}

#[tokio::test]
async fn http_call_tool_preserves_structured_content() {
    let (endpoint, server) = spawn_http_server(Arc::new(|request| {
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
