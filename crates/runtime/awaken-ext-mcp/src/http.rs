//! HTTP (Streamable) MCP transport.
//!
//! POSTs JSON-RPC requests to an MCP endpoint. The response is `application/json`
//! or an SSE `text/event-stream`; the SSE body is streamed so a call's
//! `progress` notifications are routed live and the final response is returned
//! when it arrives. Like the stdio transport it keeps the raw [`CallToolResult`]
//! (and its `isError` flag) for the three-state mapping.
//!
//! With [`connect_streaming`](HttpTransport::connect_streaming) a background GET
//! SSE listener carries the server->client stream — `tools/list_changed`,
//! `resources/updated`, and `sampling` — through the same
//! [`router`](crate::router)/[`ServerRequestHandler`] demux the stdio transport
//! uses, so remote Streamable HTTP servers get full parity.
//!
//! A [`Credential`] adds an opaque auth header and arbitrary custom headers
//! ride along on every request ([`HttpTransportBuilder::header`]); the server's
//! session id is echoed on subsequent requests.
//!
//! Auth failures (401/403) are handled managed-agents style: the credential is
//! rotatable in place ([`set_credential`](HttpTransport::set_credential)), and
//! a host-registered [`CredentialRefresher`] is consulted once per failed
//! request — on success only a classified read-only request is retried with the
//! fresh credential; effectful or unknown methods surface the original challenge
//! without replay. Refresh failure likewise surfaces the challenge (including `WWW-Authenticate`) as a
//! [`McpTransportError::ServerError`] prefixed with `auth challenge:`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use awaken_mcp_wire::McpTransportError;
use awaken_mcp_wire::{
    CallToolParams, CallToolResult, CreateTaskResult, InitializeParams, InitializeResult,
    ListToolsResult, McpTask, McpToolDefinition, TaskIdParams, TaskMetadata,
};
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::credential::{
    AuthChallenge, AuthRetrySafety, Credential, CredentialRefresher, auth_retry_safety,
};
use crate::jsonrpc::{ServerNotification, ServerRequestHandler, server_request_reply};
use crate::progress::McpProgressUpdate;
use crate::router::{NotificationSinks, route};
use crate::sse::SseParser;
use crate::transport::{ListChangedKind, McpToolTransport};
use crate::types::{
    ListPromptsResult, ListResourcesResult, McpPromptDefinition, McpPromptResult,
    McpResourceDefinition,
};

const SESSION_HEADER: &str = "Mcp-Session-Id";

/// State shared between the request path and the background SSE listener.
struct HttpShared {
    client: reqwest::Client,
    url: String,
    /// Rotatable: `set_credential` and a successful refresh replace it in
    /// place, so in-flight retries and the SSE listener pick up the new value.
    credential: RwLock<Credential>,
    /// Custom headers sent on every request, alongside the credential header.
    headers: Vec<(String, String)>,
    /// Host hook consulted once per 401/403 before the failure is surfaced.
    refresher: Option<Arc<dyn CredentialRefresher>>,
    /// Deadline for control-plane requests such as initialize and discovery.
    control_request_timeout: Duration,
    /// Independently bounded deadline for effectful `tools/call` requests.
    tool_call_timeout: Duration,
    session_id: Mutex<Option<String>>,
    sinks: Arc<NotificationSinks>,
    request_handler: Option<Arc<dyn ServerRequestHandler>>,
    /// JSON-RPC responses may arrive on the standalone GET stream rather than
    /// the request's POST body. One request id owns one response sender.
    pending_responses: tokio::sync::Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    /// Connection health. Starts `true`; a connection-level transport failure
    /// (refused / reset / DNS / timeout on `send`) latches it `false`, while a
    /// server that ANSWERS — even with a protocol/server error status — restores
    /// it `true`. Read by [`HttpTransport::is_alive`] so a dead server can be
    /// reaped, unlike the previous always-alive default.
    alive: AtomicBool,
    /// Frozen task protocol capabilities from the exact initialized MCP
    /// connection. A tool declaration never grants these operations by itself.
    task_tools_call: AtomicBool,
    task_cancel: AtomicBool,
}

impl HttpShared {
    /// Record the outcome of a `send`: `Ok(_)` (a response arrived, any status)
    /// keeps the connection alive; `Err(_)` (no response at all) latches it dead.
    fn record_send<T, E>(&self, result: Result<T, E>) -> Result<T, E> {
        self.alive.store(result.is_ok(), Ordering::SeqCst);
        result
    }
}

impl HttpShared {
    /// Custom headers + credential header + session header, on any request.
    fn apply_headers(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        if let Some((name, value)) = self.credential.read().unwrap().header() {
            request = request.header(name, value);
        }
        if let Some(session) = self.session_id.lock().unwrap().clone() {
            request = request.header(SESSION_HEADER, session);
        }
        request
    }

    /// Build a POST with all headers applied.
    fn post_builder(&self, body: &Value, timeout: Duration) -> reqwest::RequestBuilder {
        self.apply_headers(
            self.client
                .post(&self.url)
                .timeout(timeout)
                .header("Accept", "application/json, text/event-stream")
                .json(body),
        )
    }

    /// Ask the host refresher for a fresh credential; store it on success.
    async fn try_refresh(&self, challenge: &AuthChallenge) -> bool {
        let Some(refresher) = &self.refresher else {
            return false;
        };
        match refresher.refresh(challenge).await {
            Some(fresh) => {
                *self.credential.write().unwrap() = fresh;
                true
            }
            None => false,
        }
    }

    fn capture_session(&self, response: &reqwest::Response) {
        if let Some(session) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().unwrap() = Some(session.to_string());
        }
    }

    /// Route a server->client message: a notification into the sinks, a request
    /// through the handler with its reply POSTed back.
    async fn dispatch_incoming(self: &Arc<Self>, value: Value) {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = value.get("id").filter(|v| !v.is_null()).cloned();
        if method.is_none()
            && let Some(response_id) = id.as_ref().and_then(Value::as_i64)
            && let Some(sender) = self.pending_responses.lock().await.remove(&response_id)
        {
            let _ = sender.send(value);
            return;
        }
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                let reply =
                    server_request_reply(self.request_handler.as_ref(), &id, &method, params).await;
                // Fire-and-forget reply.
                let _ = self
                    .post_builder(&reply, self.control_request_timeout)
                    .send()
                    .await;
            }
            (Some(method), None) => {
                route(
                    ServerNotification {
                        method,
                        params: value.get("params").cloned().unwrap_or(Value::Null),
                    },
                    &self.sinks,
                )
                .await;
            }
            _ => {}
        }
    }

    /// Decode one complete SSE data field. A matching response terminates the
    /// request; every other valid peer message follows the existing router.
    async fn route_sse_data(
        self: &Arc<Self>,
        data: &str,
        id: i64,
    ) -> Option<Result<Value, McpTransportError>> {
        let value = match serde_json::from_str::<Value>(data) {
            Ok(value) => value,
            Err(error) => return Some(Err(McpTransportError::ProtocolError(error.to_string()))),
        };
        if value.get("method").is_none() {
            return Some(match value.get("id").and_then(Value::as_i64) {
                Some(response_id) if response_id == id => Ok(value),
                response_id => Err(McpTransportError::ProtocolError(format!(
                    "MCP response id {response_id:?} does not match request id {id}"
                ))),
            });
        }
        self.dispatch_incoming(value).await;
        None
    }

    /// Send `body` and return the JSON-RPC message that answers request `id`.
    /// Notifications and server requests seen in an SSE response are routed as
    /// they arrive, so a call's progress streams live.
    async fn request_post(
        self: &Arc<Self>,
        body: &Value,
        id: i64,
        timeout: Duration,
    ) -> Result<Option<Value>, McpTransportError> {
        let mut response = self
            .record_send(self.post_builder(body, timeout).send().await)
            .map_err(|e| McpTransportError::TransportError(e.to_string()))?;
        // Auth failure: rotate once, but resend only the explicit read-only
        // allowlist. A 401/403 is not proof that an opaque tools/call or vendor
        // method had no effect upstream.
        if is_auth_failure(response.status()) {
            let challenge = challenge_from(&response);
            if !self.try_refresh(&challenge).await {
                return Err(unauthorized_error(&challenge));
            }
            if auth_retry_safety(body) == AuthRetrySafety::Never {
                return Err(unauthorized_error(&challenge));
            }
            response = self
                .record_send(self.post_builder(body, timeout).send().await)
                .map_err(|e| McpTransportError::TransportError(e.to_string()))?;
            if is_auth_failure(response.status()) {
                return Err(unauthorized_error(&challenge_from(&response)));
            }
        }
        self.capture_session(&response);
        let status = response.status();
        let is_sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.contains("text/event-stream"));

        if !is_sse {
            let text = response
                .text()
                .await
                .map_err(|e| McpTransportError::TransportError(e.to_string()))?;
            if !status.is_success() {
                return Err(McpTransportError::ServerError(format!(
                    "HTTP {status}: {text}"
                )));
            }
            return Ok(Some(parse_sse_or_json(&text)?));
        }

        if !status.is_success() {
            return Err(McpTransportError::ServerError(format!("HTTP {status}")));
        }
        // Stream the SSE body: route notifications/requests, return the response.
        let mut stream = response.bytes_stream();
        let mut parser = SseParser::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| McpTransportError::TransportError(e.to_string()))?;
            let text = String::from_utf8_lossy(&chunk);
            for data in parser.push(&text) {
                if let Some(result) = self.route_sse_data(&data, id).await {
                    return result.map(Some);
                }
            }
        }
        // Streamable HTTP peers may end a chunked response immediately after a
        // complete `data:` line. EOF is the delimiter in that legal shape, so
        // flush the sole parser before reporting a missing response.
        for data in parser.finish() {
            if let Some(result) = self.route_sse_data(&data, id).await {
                return result.map(Some);
            }
        }
        Ok(None)
    }

    /// Send one request. The response may legally arrive either in the POST
    /// body or on the already-open standalone GET stream; both routes resolve
    /// this request's single pending-response slot.
    async fn request(
        self: &Arc<Self>,
        body: &Value,
        id: i64,
        timeout: Duration,
    ) -> Result<Value, McpTransportError> {
        let (sender, mut receiver) = oneshot::channel();
        self.pending_responses.lock().await.insert(id, sender);
        let direct = self.request_post(body, id, timeout);
        tokio::pin!(direct);
        let mut streamed = None;
        let direct_result = tokio::select! {
            result = &mut direct => Some(result),
            result = &mut receiver => {
                streamed = Some(result);
                None
            }
        };
        let raw = match (direct_result, streamed) {
            (Some(Ok(Some(value))), _) => Ok(value),
            (Some(Ok(None)), _) => tokio::time::timeout(timeout, receiver)
                .await
                .map_err(|_| {
                    McpTransportError::TransportError(
                        "SSE stream ended before the response".to_string(),
                    )
                })?
                .map_err(|_| {
                    McpTransportError::TransportError("MCP response route closed".to_string())
                }),
            (Some(Err(error)), _) => Err(error),
            (None, Some(Ok(value))) => Ok(value),
            (None, Some(Err(_))) => Err(McpTransportError::TransportError(
                "MCP response route closed".to_string(),
            )),
            (None, None) => unreachable!("select records one completed response route"),
        };
        self.pending_responses.lock().await.remove(&id);
        extract_result(raw?)
    }
}

fn is_auth_failure(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN
}

fn challenge_from(response: &reqwest::Response) -> AuthChallenge {
    AuthChallenge {
        status: response.status().as_u16(),
        www_authenticate: response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    }
}

fn unauthorized_error(challenge: &AuthChallenge) -> McpTransportError {
    McpTransportError::ServerError(format!("auth challenge: {challenge}"))
}

/// Assembles an [`HttpTransport`]: URL plus optional credential, custom
/// headers, host [`CredentialRefresher`], and sampling handler.
pub struct HttpTransportBuilder {
    url: String,
    credential: Credential,
    headers: Vec<(String, String)>,
    refresher: Option<Arc<dyn CredentialRefresher>>,
    sampling: Option<Arc<dyn crate::sampling::SamplingHandler>>,
    control_request_timeout: Duration,
    tool_call_timeout: Duration,
}

impl HttpTransportBuilder {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            credential: Credential::None,
            headers: Vec::new(),
            refresher: None,
            sampling: None,
            control_request_timeout: crate::stdio::DEFAULT_TIMEOUT,
            tool_call_timeout: crate::stdio::DEFAULT_TIMEOUT,
        }
    }

    /// Authenticate with `credential` (also rotatable later via
    /// [`HttpTransport::set_credential`]).
    pub fn credential(mut self, credential: Credential) -> Self {
        self.credential = credential;
        self
    }

    /// Add a custom header sent on every request (repeatable).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Register the host hook consulted on 401/403.
    pub fn refresher(mut self, refresher: Arc<dyn CredentialRefresher>) -> Self {
        self.refresher = Some(refresher);
        self
    }

    /// Handle server-initiated `sampling/createMessage` (streaming mode only).
    pub fn sampling(mut self, sampling: Arc<dyn crate::sampling::SamplingHandler>) -> Self {
        self.sampling = Some(sampling);
        self
    }

    /// Override the deadline for `tools/call` without lengthening initialize,
    /// discovery, notifications, or other control-plane requests.
    pub fn tool_call_timeout(mut self, timeout: Duration) -> Self {
        self.tool_call_timeout = timeout;
        self
    }

    /// Build without the MCP handshake, for callers that defer `initialize`.
    pub fn build(self) -> HttpTransport {
        let handler = self.sampling.map(|s| {
            Arc::new(crate::sampling::SamplingBridge::new(s)) as Arc<dyn ServerRequestHandler>
        });
        HttpTransport {
            shared: Arc::new(HttpShared {
                client: http_client_for(&self.url),
                url: self.url,
                credential: RwLock::new(self.credential),
                headers: self.headers,
                refresher: self.refresher,
                control_request_timeout: self.control_request_timeout,
                tool_call_timeout: self.tool_call_timeout,
                session_id: Mutex::new(None),
                sinks: Arc::new(NotificationSinks::new()),
                request_handler: handler,
                pending_responses: tokio::sync::Mutex::new(HashMap::new()),
                alive: AtomicBool::new(true),
                task_tools_call: AtomicBool::new(false),
                task_cancel: AtomicBool::new(false),
            }),
            next_id: AtomicI64::new(1),
            next_progress_token: AtomicI64::new(1),
            listener: Mutex::new(None),
        }
    }

    /// Connect (handshake only), for simple request/response use.
    pub async fn connect(self) -> Result<HttpTransport, McpTransportError> {
        let transport = self.build();
        transport.initialize().await?;
        Ok(transport)
    }

    /// Connect and start the background GET SSE listener, so server->client
    /// streams (list_changed, resources/updated, sampling) are delivered.
    pub async fn connect_streaming(self) -> Result<HttpTransport, McpTransportError> {
        let transport = self.build();
        transport.initialize().await?;
        transport.spawn_listener();
        Ok(transport)
    }
}

fn http_client_for(url: &str) -> reqwest::Client {
    let mut builder = reqwest::Client::builder();
    if reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
    {
        builder = builder.no_proxy();
    }
    builder.build().expect("build MCP HTTP client")
}

/// An HTTP MCP server connection presented as an [`McpToolTransport`].
pub struct HttpTransport {
    shared: Arc<HttpShared>,
    next_id: AtomicI64,
    next_progress_token: AtomicI64,
    /// Background GET SSE listener; aborted on drop.
    listener: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for HttpTransport {
    fn drop(&mut self) {
        if let Some(handle) = self.listener.lock().unwrap().take() {
            handle.abort();
        }
    }
}

impl HttpTransport {
    /// Build a transport for `url`, authenticating with `credential`.
    /// For custom headers or a refresher, use [`HttpTransportBuilder`].
    pub fn new(url: impl Into<String>, credential: Credential) -> Self {
        HttpTransportBuilder::new(url)
            .credential(credential)
            .build()
    }

    /// Connect (handshake only), for simple request/response use.
    pub async fn connect(
        url: impl Into<String>,
        credential: Credential,
    ) -> Result<Self, McpTransportError> {
        HttpTransportBuilder::new(url)
            .credential(credential)
            .connect()
            .await
    }

    /// Connect and start the background GET SSE listener, so server->client
    /// streams (list_changed, resources/updated, sampling) are delivered.
    pub async fn connect_streaming(
        url: impl Into<String>,
        credential: Credential,
        sampling: Option<Arc<dyn crate::sampling::SamplingHandler>>,
    ) -> Result<Self, McpTransportError> {
        let mut builder = HttpTransportBuilder::new(url).credential(credential);
        if let Some(sampling) = sampling {
            builder = builder.sampling(sampling);
        }
        builder.connect_streaming().await
    }

    /// Replace the credential used from the next request on — the host calls
    /// this when its vault rotates a token outside the 401 path.
    pub fn set_credential(&self, credential: Credential) {
        *self.shared.credential.write().unwrap() = credential;
    }

    fn spawn_listener(&self) {
        let shared = Arc::clone(&self.shared);
        let handle = tokio::spawn(async move {
            listen(shared).await;
        });
        *self.listener.lock().unwrap() = Some(handle);
    }

    async fn initialize(&self) -> Result<(), McpTransportError> {
        let params = InitializeParams::new(None);
        let value = self
            .request("initialize", serde_json::to_value(&params)?)
            .await?;
        let result: InitializeResult = serde_json::from_value(value)?;
        let tasks = result.capabilities.tasks.unwrap_or_default();
        self.shared
            .task_tools_call
            .store(tasks.supports_tool_call(), Ordering::SeqCst);
        self.shared
            .task_cancel
            .store(tasks.supports_cancel(), Ordering::SeqCst);
        // `notifications/initialized` is a fire-and-forget notification.
        let _ = self
            .shared
            .post_builder(
                &json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }),
                self.shared.control_request_timeout,
            )
            .send()
            .await;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpTransportError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = build_request_body(id, method, params);
        let timeout = if method == "tools/call" {
            self.shared.tool_call_timeout
        } else {
            self.shared.control_request_timeout
        };
        self.shared.request(&body, id, timeout).await
    }

    /// Subscribe to `list_changed` signals.
    pub fn subscribe_list_changed(&self) -> broadcast::Receiver<ListChangedKind> {
        self.shared.sinks.list_changed.subscribe()
    }

    /// Subscribe to `resources/updated` uris.
    pub fn subscribe_resource_updated(&self) -> broadcast::Receiver<String> {
        self.shared.sinks.resource_updated.subscribe()
    }
}

/// Background GET SSE listener: reconnects on stream end and routes every
/// server->client message. Runs until the task is aborted (on transport drop).
async fn listen(shared: Arc<HttpShared>) {
    loop {
        let request = shared.apply_headers(
            shared
                .client
                .get(&shared.url)
                .header("Accept", "text/event-stream"),
        );
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                let mut stream = response.bytes_stream();
                let mut parser = SseParser::new();
                while let Some(Ok(chunk)) = stream.next().await {
                    let text = String::from_utf8_lossy(&chunk);
                    dispatch_sse_events(&shared, parser.push(&text)).await;
                }
                // The peer may close immediately after a complete data line.
                // Route that final event through the same request-id demux as
                // delimiter-terminated events before reconnecting.
                dispatch_sse_events(&shared, parser.finish()).await;
            }
            // Auth failure: reconnect only if the host refresher produces a
            // fresh credential; otherwise stop rather than hammer the server.
            Ok(response) if is_auth_failure(response.status()) => {
                if !shared.try_refresh(&challenge_from(&response)).await {
                    return;
                }
            }
            // A server without a standalone GET stream (405/404) — stop listening.
            Ok(_) => return,
            Err(_) => {}
        }
        // Brief backoff before reconnecting.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn dispatch_sse_events(shared: &Arc<HttpShared>, events: Vec<String>) {
    for data in events {
        if let Ok(value) = serde_json::from_str::<Value>(&data) {
            shared.dispatch_incoming(value).await;
        }
    }
}

/// Build a JSON-RPC request body.
fn build_request_body(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// Parse a fully-buffered body that is either JSON or SSE, returning the
/// JSON-RPC message.
fn parse_sse_or_json(text: &str) -> Result<Value, McpTransportError> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return serde_json::from_str(trimmed)
            .map_err(|e| McpTransportError::ProtocolError(e.to_string()));
    }
    let mut parser = SseParser::new();
    let mut events = parser.push(text);
    events.extend(parser.finish());
    events
        .into_iter()
        .rev()
        .find_map(|data| serde_json::from_str::<Value>(&data).ok())
        .ok_or_else(|| McpTransportError::ProtocolError("no JSON in SSE response".to_string()))
}

/// Extract the `result` from a JSON-RPC response, or map `error` to an error.
fn extract_result(value: Value) -> Result<Value, McpTransportError> {
    if let Some(error) = value.get("error") {
        return Err(McpTransportError::ServerError(error.to_string()));
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

#[async_trait]
impl McpToolTransport for HttpTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        let result = self.request("tools/list", json!({})).await?;
        let parsed: ListToolsResult = serde_json::from_value(result)?;
        Ok(parsed.tools)
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpTransportError> {
        let params = CallToolParams {
            name: tool_name.to_string(),
            arguments: Some(arguments),
            task: None,
            meta: None,
        };
        let result = self
            .request("tools/call", serde_json::to_value(&params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    async fn call_tool_with_progress(
        &self,
        tool_name: &str,
        arguments: Value,
        progress_tx: tokio::sync::mpsc::Sender<McpProgressUpdate>,
    ) -> Result<CallToolResult, McpTransportError> {
        let token = self.next_progress_token.fetch_add(1, Ordering::SeqCst);
        self.shared
            .sinks
            .progress
            .lock()
            .await
            .insert(token, progress_tx);
        let params = CallToolParams {
            name: tool_name.to_string(),
            arguments: Some(arguments),
            task: None,
            meta: Some(json!({ "progressToken": token })),
        };
        let result = self
            .request("tools/call", serde_json::to_value(&params)?)
            .await;
        self.shared.sinks.progress.lock().await.remove(&token);
        Ok(serde_json::from_value(result?)?)
    }

    fn supports_task_tools_call(&self) -> bool {
        self.shared.task_tools_call.load(Ordering::SeqCst)
    }

    fn supports_task_cancel(&self) -> bool {
        self.shared.task_cancel.load(Ordering::SeqCst)
    }

    async fn call_tool_as_task(
        &self,
        tool_name: &str,
        arguments: Value,
        ttl_ms: Option<u64>,
    ) -> Result<CreateTaskResult, McpTransportError> {
        if !self.supports_task_tools_call() {
            return Err(McpTransportError::NotSupported(
                "task-augmented tools/call".into(),
            ));
        }
        let params = CallToolParams {
            name: tool_name.to_string(),
            arguments: Some(arguments),
            task: Some(TaskMetadata { ttl: ttl_ms }),
            meta: None,
        };
        let result = self
            .request("tools/call", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    async fn get_task(&self, task_id: &str) -> Result<McpTask, McpTransportError> {
        let params = TaskIdParams {
            task_id: task_id.to_string(),
        };
        let result = self
            .request("tasks/get", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    async fn get_task_result(&self, task_id: &str) -> Result<CallToolResult, McpTransportError> {
        let params = TaskIdParams {
            task_id: task_id.to_string(),
        };
        let result = self
            .request("tasks/result", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    async fn cancel_task(&self, task_id: &str) -> Result<McpTask, McpTransportError> {
        if !self.supports_task_cancel() {
            return Err(McpTransportError::NotSupported("tasks/cancel".into()));
        }
        let params = TaskIdParams {
            task_id: task_id.to_string(),
        };
        let result = self
            .request("tasks/cancel", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
        let result = self.request("prompts/list", json!({})).await?;
        let parsed: ListPromptsResult = serde_json::from_value(result)?;
        Ok(parsed.prompts)
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<McpPromptResult, McpTransportError> {
        let result = self
            .request(
                "prompts/get",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
        let result = self.request("resources/list", json!({})).await?;
        let parsed: ListResourcesResult = serde_json::from_value(result)?;
        Ok(parsed.resources)
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, McpTransportError> {
        self.request("resources/read", json!({ "uri": uri })).await
    }

    /// Report the tracked connection health rather than the always-true default:
    /// once a request hits a connection-level transport failure the transport is
    /// flagged dead so the manager can reap it (a server that merely answers with
    /// an error status stays alive).
    fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_is_jsonrpc_2() {
        let body = build_request_body(3, "tools/list", json!({}));
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 3);
        assert_eq!(body["method"], "tools/list");
    }

    #[test]
    fn parses_a_plain_json_response() {
        let value =
            parse_sse_or_json(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).expect("parses");
        assert_eq!(value["result"]["ok"], true);
    }

    #[test]
    fn parses_an_sse_response() {
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"n\":2}}\n\n";
        let value = parse_sse_or_json(sse).expect("parses");
        assert_eq!(value["result"]["n"], 2);
    }

    #[test]
    fn sse_without_json_is_a_protocol_error() {
        assert!(parse_sse_or_json("event: ping\n\n").is_err());
    }

    #[test]
    fn extract_result_returns_result() {
        let value = json!({ "jsonrpc": "2.0", "id": 1, "result": { "a": 1 } });
        assert_eq!(extract_result(value).unwrap()["a"], 1);
    }

    #[test]
    fn extract_result_maps_error_to_err() {
        let value =
            json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -1, "message": "boom" } });
        let err = extract_result(value).expect_err("errors");
        assert!(matches!(err, McpTransportError::ServerError(_)));
    }

    #[tokio::test]
    async fn dispatch_routes_a_notification_to_the_sinks() {
        let shared = Arc::clone(&HttpTransportBuilder::new("http://unused").build().shared);
        let mut rx = shared.sinks.list_changed.subscribe();
        shared
            .dispatch_incoming(json!({
                "jsonrpc": "2.0", "method": "notifications/tools/list_changed"
            }))
            .await;
        assert_eq!(rx.recv().await.unwrap(), ListChangedKind::Tools);
    }

    // ---- auth mechanics against a local single-shot HTTP server ----

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const EMPTY_TOOLS: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;

    fn ok_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn unauthorized_response() -> String {
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"mcp\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string()
    }

    fn accepted_response() -> String {
        "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
    }

    fn task_value(status: &str) -> Value {
        json!({
            "taskId": "remote-http",
            "status": status,
            "createdAt": "2026-08-30T00:00:00Z",
            "lastUpdatedAt": "2026-08-30T00:00:01Z",
            "ttl": 10_000,
            "pollInterval": 50
        })
    }

    /// Serve one canned response per accepted connection; returns the base URL
    /// and a handle resolving to the raw request bytes each connection sent.
    async fn serve(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        serve_after(
            responses
                .into_iter()
                .map(|response| (Duration::ZERO, response))
                .collect(),
        )
        .await
    }

    async fn serve_after(
        responses: Vec<(Duration, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let mut captured = Vec::new();
            for (delay, response) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                // Read until the blank line; these requests have small bodies
                // that arrive in the same segments.
                loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                captured.push(String::from_utf8_lossy(&request).to_string());
                tokio::time::sleep(delay).await;
                if delay.is_zero() {
                    socket.write_all(response.as_bytes()).await.unwrap();
                } else {
                    // A timeout case intentionally drops the client before the
                    // delayed fixture writes; the accepted request still proves
                    // the transport sent exactly once.
                    let _ = socket.write_all(response.as_bytes()).await;
                }
                socket.shutdown().await.ok();
            }
            captured
        });
        (url, handle)
    }

    #[tokio::test]
    async fn negotiated_task_methods_follow_one_typed_http_path() {
        // Cause/effect graph: C1=initialize advertises task tools/call and
        // cancel; C2=the same HTTP connection issues create/get/result/cancel;
        // C3=each response has its protocol-specific shape. Effects E1=the two
        // gates are cached; E2=create/get/cancel decode as Task; E3=result
        // decodes as the original CallToolResult. Decision rule H1=C1+C2+C3 =>
        // E1+E2+E3. No tasks/* method enters the runtime descriptor catalog.
        let initialize = json!({
            "jsonrpc": "2.0", "id": 1, "result": {
                "protocolVersion": "2025-11-25",
                "capabilities": {"tasks": {
                    "cancel": {},
                    "requests": {"tools": {"call": {}}}
                }},
                "serverInfo": {"name": "tasks", "version": "1"}
            }
        });
        let created = json!({
            "jsonrpc": "2.0", "id": 2,
            "result": {"task": task_value("working")}
        });
        let status = json!({
            "jsonrpc": "2.0", "id": 3,
            "result": task_value("completed")
        });
        let result = json!({
            "jsonrpc": "2.0", "id": 4,
            "result": {"content": [{"type": "text", "text": "done"}], "isError": false}
        });
        let cancelled = json!({
            "jsonrpc": "2.0", "id": 5,
            "result": task_value("cancelled")
        });
        let (url, server) = serve(vec![
            ok_response(&initialize.to_string()),
            accepted_response(),
            ok_response(&created.to_string()),
            ok_response(&status.to_string()),
            ok_response(&result.to_string()),
            ok_response(&cancelled.to_string()),
        ])
        .await;

        let transport = HttpTransportBuilder::new(url)
            .connect()
            .await
            .expect("H1 initializes");
        assert!(transport.supports_task_tools_call(), "H1/E1");
        assert!(transport.supports_task_cancel(), "H1/E1");
        assert_eq!(
            transport
                .call_tool_as_task("slow", json!({}), Some(10_000))
                .await
                .unwrap()
                .task
                .task_id,
            "remote-http",
            "H1/E2"
        );
        assert_eq!(
            transport.get_task("remote-http").await.unwrap().status,
            awaken_mcp_wire::TaskStatus::Completed,
            "H1/E2"
        );
        assert_eq!(
            transport
                .get_task_result("remote-http")
                .await
                .unwrap()
                .content
                .len(),
            1,
            "H1/E3"
        );
        assert_eq!(
            transport.cancel_task("remote-http").await.unwrap().status,
            awaken_mcp_wire::TaskStatus::Cancelled,
            "H1/E2"
        );
        assert_eq!(
            server.await.unwrap().len(),
            6,
            "H1 one request per operation"
        );
    }

    struct StaticRefresher {
        fresh: Option<Credential>,
        seen: Mutex<Vec<AuthChallenge>>,
    }

    #[async_trait]
    impl CredentialRefresher for StaticRefresher {
        async fn refresh(&self, challenge: &AuthChallenge) -> Option<Credential> {
            self.seen.lock().unwrap().push(challenge.clone());
            self.fresh.clone()
        }
    }

    #[tokio::test]
    async fn custom_headers_and_credential_ride_on_every_request() {
        let (url, server) = serve(vec![ok_response(EMPTY_TOOLS)]).await;
        let transport = HttpTransportBuilder::new(url)
            .credential(Credential::Bearer("tok".to_string()))
            .header("X-Org-Id", "org-42")
            .build();
        transport.list_tools().await.expect("tools/list succeeds");
        let captured = server.await.unwrap();
        let request = captured[0].to_ascii_lowercase();
        assert!(request.contains("authorization: bearer tok"), "{request}");
        assert!(request.contains("x-org-id: org-42"), "{request}");
    }

    #[tokio::test]
    async fn control_and_tool_calls_follow_independent_timeout_decision_rules() {
        // Causes: C1 request is control-plane or tools/call; C2 the response
        // arrives after the short control deadline; C3 it arrives before the
        // longer tool deadline. Effects: E1 control remains bounded by the short
        // deadline; E2 a valid long tool call succeeds; E3 a call beyond its own
        // deadline fails without transport replay. Constraints: tool timeout
        // override never changes the control default, and every rule sends one
        // upstream request. Rules: T1 control+C2 -> E1; T2 tool+C2+C3 -> E2;
        // T3 tool+C2+!C3 -> E3. FMECA: one shared 30s deadline truncates committed
        // long-running tools, while automatic replay can duplicate their effects.
        const SHORT: Duration = Duration::from_millis(25);
        const BETWEEN: Duration = Duration::from_millis(100);
        const LONG: Duration = Duration::from_millis(500);

        let defaults = HttpTransportBuilder::new("http://unused");
        assert_eq!(
            defaults.control_request_timeout,
            crate::stdio::DEFAULT_TIMEOUT
        );
        assert_eq!(defaults.tool_call_timeout, crate::stdio::DEFAULT_TIMEOUT);

        let (url, server) = serve_after(vec![(BETWEEN, ok_response(EMPTY_TOOLS))]).await;
        let mut builder = HttpTransportBuilder::new(url).tool_call_timeout(LONG);
        assert_eq!(
            builder.control_request_timeout,
            crate::stdio::DEFAULT_TIMEOUT
        );
        builder.control_request_timeout = SHORT;
        let control_error = builder
            .build()
            .list_tools()
            .await
            .expect_err("T1/E1: slow control request times out");
        assert!(
            matches!(control_error, McpTransportError::TransportError(_)),
            "{control_error:?}"
        );
        assert_eq!(server.await.unwrap().len(), 1, "T1 sends once");

        let call_body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"done"}],"isError":false}}"#;
        let (url, server) = serve_after(vec![(BETWEEN, ok_response(call_body))]).await;
        let mut builder = HttpTransportBuilder::new(url).tool_call_timeout(LONG);
        builder.control_request_timeout = SHORT;
        builder
            .build()
            .call_tool("slow", json!({}))
            .await
            .expect("T2/E2: tool uses the longer deadline");
        assert_eq!(server.await.unwrap().len(), 1, "T2 sends once");

        let (url, server) = serve_after(vec![(BETWEEN, ok_response(call_body))]).await;
        let tool_error = HttpTransportBuilder::new(url)
            .tool_call_timeout(SHORT)
            .build()
            .call_tool("too-slow", json!({}))
            .await
            .expect_err("T3/E3: tool remains bounded by its own deadline");
        assert!(
            matches!(tool_error, McpTransportError::TransportError(_)),
            "{tool_error:?}"
        );
        assert_eq!(server.await.unwrap().len(), 1, "T3 never replays");
    }

    #[tokio::test]
    async fn sse_eof_flush_follows_the_stream_termination_decision_table() {
        // Causes: C1 response is SSE; C2 matching JSON-RPC id is complete;
        // C3 a trailing blank event delimiter is present. Effect E1 returns the
        // committed response; E2 reports a transport error. Constraints: C2 is
        // mandatory, while C3 is optional because EOF terminates the last SSE
        // event. Decision rules: S1 C1+C2+C3 -> E1 (covered by the ordinary SSE
        // tests); S2 C1+C2+!C3 -> E1 (this regression); S3 C1+!C2 -> E2.
        // FMECA: dropping S2 makes a successful side-effecting tool call appear
        // failed, encourages an unsafe duplicate retry, and leaves a valid MCP
        // Session to be misdiagnosed as lost.
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, server) = serve(vec![response]).await;
        let transport = HttpTransportBuilder::new(url).build();
        assert!(transport.list_tools().await.expect("S2/E1").is_empty());
        server.await.expect("fixture completes");
    }

    #[tokio::test]
    async fn get_stream_response_follows_the_dual_response_route_decision_table() {
        // Causes: C1 POST carries the matching response; C2 standalone GET
        // carries it; C3 neither route carries it before the timeout. Exactly
        // one of C1/C2 may satisfy the request. Effects: E1 resolve once through
        // the request-id slot; E2 return the bounded transport error. Rules:
        // G1 C1+!C2 -> E1 (ordinary HTTP/SSE tests); G2 !C1+C2 -> E1 (this
        // regression); G3 !C1+!C2+C3 -> E2 (existing missing-response path).
        // FMECA: missing G2 drops a successful side-effecting tool result, after
        // which an agent may duplicate the effect and reuse a stale MCP Session.
        let empty_sse = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
        let (url, server) = serve(vec![empty_sse]).await;
        let transport = HttpTransportBuilder::new(url).build();
        let shared = Arc::clone(&transport.shared);
        let routed = tokio::spawn(async move {
            loop {
                if shared.pending_responses.lock().await.contains_key(&1) {
                    shared
                        .dispatch_incoming(json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "result": { "tools": [] }
                        }))
                        .await;
                    return;
                }
                tokio::task::yield_now().await;
            }
        });
        assert!(transport.list_tools().await.expect("G2/E1").is_empty());
        routed.await.expect("GET response routed");
        server.await.expect("POST fixture completes");
    }

    #[tokio::test]
    async fn get_stream_eof_routes_the_pending_response_decision_rule() {
        // Causes: C1 the standalone GET event carries the pending request id;
        // C2 its final data line lacks a blank delimiter; C3 the stream ends.
        // Effect E1 resolves the single pending-response slot. Rule L1
        // C1+C2+C3 -> E1. Constraint: POST and GET share that one slot, so the
        // response cannot be committed twice. FMECA: omitting the GET EOF rule
        // reports a false transport failure after the remote tool side effect,
        // then retries against a stale or closed MCP Session.
        let transport = HttpTransportBuilder::new("http://unused").build();
        let shared = Arc::clone(&transport.shared);
        let (sender, receiver) = oneshot::channel();
        shared.pending_responses.lock().await.insert(7, sender);
        let mut parser = SseParser::new();
        assert!(
            parser
                .push(
                    "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}"
                )
                .is_empty()
        );
        dispatch_sse_events(&shared, parser.finish()).await;
        assert_eq!(receiver.await.expect("L1/E1")["result"]["ok"], true);
        assert!(shared.pending_responses.lock().await.is_empty());
    }

    #[tokio::test]
    async fn refreshes_and_retries_once_on_401() {
        let (url, server) = serve(vec![unauthorized_response(), ok_response(EMPTY_TOOLS)]).await;
        let refresher = Arc::new(StaticRefresher {
            fresh: Some(Credential::Bearer("fresh".to_string())),
            seen: Mutex::new(Vec::new()),
        });
        let transport = HttpTransportBuilder::new(url)
            .credential(Credential::Bearer("stale".to_string()))
            .refresher(Arc::clone(&refresher) as Arc<dyn CredentialRefresher>)
            .build();
        transport.list_tools().await.expect("retry succeeds");

        let captured = server.await.unwrap();
        assert!(captured[0].to_ascii_lowercase().contains("bearer stale"));
        assert!(captured[1].to_ascii_lowercase().contains("bearer fresh"));
        let seen = refresher.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].status, 401);
        assert_eq!(
            seen[0].www_authenticate.as_deref(),
            Some("Bearer realm=\"mcp\"")
        );
    }

    #[tokio::test]
    async fn auth_failure_without_refresher_surfaces_the_challenge() {
        let (url, _server) = serve(vec![unauthorized_response()]).await;
        let transport = HttpTransportBuilder::new(url).build();
        let err = transport.list_tools().await.expect_err("401 surfaces");
        let McpTransportError::ServerError(message) = err else {
            panic!("expected ServerError, got {err:?}");
        };
        assert!(message.starts_with("auth challenge: HTTP 401"), "{message}");
        assert!(message.contains("Bearer realm=\"mcp\""), "{message}");
    }

    #[tokio::test]
    async fn failed_refresh_surfaces_the_challenge() {
        let (url, _server) = serve(vec![unauthorized_response()]).await;
        let refresher = Arc::new(StaticRefresher {
            fresh: None,
            seen: Mutex::new(Vec::new()),
        });
        let transport = HttpTransportBuilder::new(url)
            .refresher(refresher as Arc<dyn CredentialRefresher>)
            .build();
        let err = transport.list_tools().await.expect_err("401 surfaces");
        assert!(
            err.to_string().contains("auth challenge: HTTP 401"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn retry_still_401_surfaces_the_second_challenge() {
        let second_401 =
            "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"second\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string();
        let (url, server) = serve(vec![unauthorized_response(), second_401]).await;
        let refresher = Arc::new(StaticRefresher {
            fresh: Some(Credential::Bearer("fresh".to_string())),
            seen: Mutex::new(Vec::new()),
        });
        let transport = HttpTransportBuilder::new(url)
            .credential(Credential::Bearer("stale".to_string()))
            .refresher(Arc::clone(&refresher) as Arc<dyn CredentialRefresher>)
            .build();
        let err = transport
            .list_tools()
            .await
            .expect_err("retry 401 surfaces");
        // The retry's own challenge surfaces, not the first one's.
        let message = err.to_string();
        assert!(message.contains("auth challenge: HTTP 401"), "{message}");
        assert!(message.contains("Bearer realm=\"second\""), "{message}");

        let captured = server.await.unwrap();
        assert!(captured[1].to_ascii_lowercase().contains("bearer fresh"));
        let seen = refresher.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one refresh per request");
    }

    #[tokio::test]
    async fn forbidden_403_triggers_refresh() {
        let forbidden =
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
        let (url, server) = serve(vec![forbidden, ok_response(EMPTY_TOOLS)]).await;
        let refresher = Arc::new(StaticRefresher {
            fresh: Some(Credential::Bearer("fresh".to_string())),
            seen: Mutex::new(Vec::new()),
        });
        let transport = HttpTransportBuilder::new(url)
            .credential(Credential::Bearer("stale".to_string()))
            .refresher(Arc::clone(&refresher) as Arc<dyn CredentialRefresher>)
            .build();
        transport.list_tools().await.expect("retry succeeds");

        let captured = server.await.unwrap();
        assert!(captured[0].to_ascii_lowercase().contains("bearer stale"));
        assert!(captured[1].to_ascii_lowercase().contains("bearer fresh"));
        let seen = refresher.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].status, 403);
    }

    #[tokio::test]
    async fn a_bare_401_without_www_authenticate_surfaces() {
        let bare_401 =
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string();
        let (url, _server) = serve(vec![bare_401]).await;
        let refresher = Arc::new(StaticRefresher {
            fresh: None,
            seen: Mutex::new(Vec::new()),
        });
        let transport = HttpTransportBuilder::new(url)
            .refresher(Arc::clone(&refresher) as Arc<dyn CredentialRefresher>)
            .build();
        let err = transport.list_tools().await.expect_err("401 surfaces");
        let message = err.to_string();
        assert!(message.contains("auth challenge: HTTP 401"), "{message}");
        assert!(!message.contains("WWW-Authenticate"), "{message}");
        let seen = refresher.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].www_authenticate, None);
    }

    #[tokio::test]
    async fn sse_listener_stops_when_refresh_declines() {
        let (url, _server) = serve(vec![unauthorized_response()]).await;
        let refresher = Arc::new(StaticRefresher {
            fresh: None,
            seen: Mutex::new(Vec::new()),
        });
        let transport = HttpTransportBuilder::new(url)
            .refresher(Arc::clone(&refresher) as Arc<dyn CredentialRefresher>)
            .build();
        tokio::time::timeout(
            Duration::from_secs(5),
            listen(Arc::clone(&transport.shared)),
        )
        .await
        .expect("listener stops instead of hammering the server");
        let seen = refresher.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].status, 401);
    }

    #[tokio::test]
    async fn sse_listener_reconnects_after_refresh() {
        let sse = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n"
            .to_string();
        let (url, server) = serve(vec![unauthorized_response(), sse]).await;
        let refresher = Arc::new(StaticRefresher {
            fresh: Some(Credential::Bearer("fresh".to_string())),
            seen: Mutex::new(Vec::new()),
        });
        let transport = HttpTransportBuilder::new(url)
            .credential(Credential::Bearer("stale".to_string()))
            .refresher(refresher as Arc<dyn CredentialRefresher>)
            .build();
        let mut rx = transport.subscribe_list_changed();
        let listener = tokio::spawn(listen(Arc::clone(&transport.shared)));

        let kind = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("listener reconnects and routes the notification")
            .unwrap();
        assert_eq!(kind, ListChangedKind::Tools);
        listener.abort();

        let captured = server.await.unwrap();
        assert!(captured[0].to_ascii_lowercase().contains("bearer stale"));
        assert!(captured[1].to_ascii_lowercase().contains("bearer fresh"));
    }

    #[tokio::test]
    async fn set_credential_rotates_the_auth_header() {
        let (url, server) = serve(vec![ok_response(EMPTY_TOOLS), ok_response(EMPTY_TOOLS)]).await;
        let transport = HttpTransportBuilder::new(url)
            .credential(Credential::Bearer("first".to_string()))
            .build();
        transport.list_tools().await.expect("first call");
        transport.set_credential(Credential::Header {
            name: "X-Api-Key".to_string(),
            value: "second".to_string(),
        });
        transport.list_tools().await.expect("second call");

        let captured = server.await.unwrap();
        assert!(captured[0].to_ascii_lowercase().contains("bearer first"));
        assert!(
            captured[1]
                .to_ascii_lowercase()
                .contains("x-api-key: second")
        );
    }

    // ---- server->client request path over the POST SSE response stream ----

    use crate::sampling::{SamplingError, SamplingHandler, SamplingRequest, SamplingResponse};

    /// Read one full HTTP request: headers, then the `Content-Length` body (the
    /// `serve` helper above reads only headers; the reply POST bodies matter here).
    async fn read_http(socket: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        let header_end = loop {
            let n = socket.read(&mut buf).await.unwrap();
            if n == 0 {
                return String::from_utf8_lossy(&request).to_string();
            }
            request.extend_from_slice(&buf[..n]);
            if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let n = socket.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&request).to_string()
    }

    /// A server that answers the first POST with an SSE body interleaving a
    /// server->client `sampling/createMessage` request (id `srv-1`) BEFORE the
    /// response to the client's request, then captures the reply POST the
    /// transport sends back. Returns the base URL and a handle to the reply body.
    async fn serve_interleaved_server_request() -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            // conn 1: the tools/list POST -> SSE (server request, then response).
            let (mut s1, _) = listener.accept().await.unwrap();
            read_http(&mut s1).await;
            let sse = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
data: {\"jsonrpc\":\"2.0\",\"id\":\"srv-1\",\"method\":\"sampling/createMessage\",\"params\":{\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}]}}\n\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
            s1.write_all(sse.as_bytes()).await.unwrap();
            s1.shutdown().await.ok();
            // conn 2: the transport POSTs the reply back; capture it.
            let (mut s2, _) = listener.accept().await.unwrap();
            let reply = read_http(&mut s2).await;
            s2.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            s2.shutdown().await.ok();
            reply
        });
        (url, handle)
    }

    struct EchoSampler;
    #[async_trait]
    impl SamplingHandler for EchoSampler {
        async fn create_message(
            &self,
            request: SamplingRequest,
        ) -> Result<SamplingResponse, SamplingError> {
            let last = request
                .messages
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default();
            Ok(SamplingResponse::assistant(format!("echo: {last}")))
        }
    }

    #[tokio::test]
    async fn a_server_sampling_request_over_http_is_handled_and_replied() {
        // A server->client `sampling/createMessage` interleaved in a POST's SSE
        // response is dispatched to the host handler, whose result is POSTed back
        // as the reply — while the client's own tools/list still resolves.
        let (url, server) = serve_interleaved_server_request().await;
        let transport = HttpTransportBuilder::new(url)
            .sampling(Arc::new(EchoSampler) as Arc<dyn SamplingHandler>)
            .build();

        let tools = transport
            .list_tools()
            .await
            .expect("tools/list resolves past the interleaved server request");
        assert!(tools.is_empty());

        let reply = server.await.unwrap();
        assert!(
            reply.contains("\"id\":\"srv-1\""),
            "the reply targets the server request id: {reply}"
        );
        assert!(
            reply.contains("\"result\""),
            "the reply is a JSON-RPC result, not an error: {reply}"
        );
        assert!(
            reply.contains("echo: ping"),
            "the reply carries the sampling handler's output: {reply}"
        );
    }

    #[tokio::test]
    async fn a_server_request_without_a_handler_fails_closed_method_not_found() {
        // Same interleave, but no sampling handler registered (request_handler is
        // None): the transport must reply with JSON-RPC -32601 (method not found)
        // rather than silently dropping the server's request.
        let (url, server) = serve_interleaved_server_request().await;
        let transport = HttpTransportBuilder::new(url).build();

        let tools = transport
            .list_tools()
            .await
            .expect("tools/list still resolves");
        assert!(tools.is_empty());

        let reply = server.await.unwrap();
        assert!(
            reply.contains("\"id\":\"srv-1\""),
            "the error reply targets the server request id: {reply}"
        );
        assert!(
            reply.contains("\"error\"") && reply.contains("-32601"),
            "fail-closed with method_not_found (-32601): {reply}"
        );
    }

    #[tokio::test]
    async fn http_transport_reports_not_alive_after_the_server_is_gone() {
        // Bind then immediately drop the listener, so the address is dead.
        let addr = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap()
        };
        let transport = HttpTransport::new(format!("http://{addr}"), Credential::None);

        // A freshly built transport is presumed alive until it observes a failure.
        assert!(transport.is_alive(), "presumed alive before any request");

        // A real call fails against the dead address (a connection-level transport
        // failure — no response ever arrives)...
        assert!(
            transport.list_tools().await.is_err(),
            "the dead server is unreachable"
        );
        // ...which latches the transport not-alive, so the manager's
        // ServerStatus.alive flags it for reaping — parity with the process-backed
        // stdio transport that reports its child's liveness.
        assert!(
            !transport.is_alive(),
            "a connection-level failure marks the transport dead"
        );
    }

    #[tokio::test]
    async fn http_transport_stays_alive_after_a_server_error_response() {
        // A server that ANSWERS — even with a non-2xx protocol/server error — is
        // reachable, so the transport must stay alive (only a connection-level
        // failure with no response marks it dead).
        let error_500 =
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                .to_string();
        let (url, _server) = serve(vec![error_500]).await;
        let transport = HttpTransport::new(url, Credential::None);
        assert!(
            transport.list_tools().await.is_err(),
            "a 500 surfaces as an error"
        );
        assert!(
            transport.is_alive(),
            "a server that answers with an error status stays alive"
        );
    }
}
