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
//! A [`Credential`] adds an opaque auth header; the server's session id is
//! echoed on subsequent requests.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use mcp::transport::McpTransportError;
use mcp::{CallToolParams, CallToolResult, InitializeParams, ListToolsResult, McpToolDefinition};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::credential::Credential;
use crate::jsonrpc::{ServerNotification, ServerRequestError, ServerRequestHandler};
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
    credential: Credential,
    timeout: Duration,
    session_id: Mutex<Option<String>>,
    sinks: Arc<NotificationSinks>,
    request_handler: Option<Arc<dyn ServerRequestHandler>>,
}

impl HttpShared {
    /// Build a POST with auth + session headers applied.
    fn post_builder(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .post(&self.url)
            .timeout(self.timeout)
            .header("Accept", "application/json, text/event-stream")
            .json(body);
        if let Some((name, value)) = self.credential.header() {
            request = request.header(name, value);
        }
        if let Some(session) = self.session_id.lock().unwrap().clone() {
            request = request.header(SESSION_HEADER, session);
        }
        request
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
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                let reply = match &self.request_handler {
                    Some(handler) => match handler.handle(&method, params).await {
                        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                        Err(err) => json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": { "code": err.code, "message": err.message },
                        }),
                    },
                    None => {
                        let err = ServerRequestError::method_not_found(&method);
                        json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": { "code": err.code, "message": err.message },
                        })
                    }
                };
                // Fire-and-forget reply.
                let _ = self.post_builder(&reply).send().await;
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

    /// Send `body` and return the JSON-RPC message that answers request `id`.
    /// Notifications and server requests seen in an SSE response are routed as
    /// they arrive, so a call's progress streams live.
    async fn request(self: &Arc<Self>, body: &Value, id: i64) -> Result<Value, McpTransportError> {
        let response = self
            .post_builder(body)
            .send()
            .await
            .map_err(|e| McpTransportError::TransportError(e.to_string()))?;
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
            return extract_result(parse_sse_or_json(&text)?);
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
                let Ok(value) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                let is_response = value.get("method").is_none()
                    && value.get("id").and_then(Value::as_i64) == Some(id);
                if is_response {
                    return extract_result(value);
                }
                self.dispatch_incoming(value).await;
            }
        }
        Err(McpTransportError::TransportError(
            "SSE stream ended before the response".to_string(),
        ))
    }
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
    pub fn new(url: impl Into<String>, credential: Credential) -> Self {
        Self::with_handler(url, credential, None)
    }

    fn with_handler(
        url: impl Into<String>,
        credential: Credential,
        request_handler: Option<Arc<dyn ServerRequestHandler>>,
    ) -> Self {
        Self {
            shared: Arc::new(HttpShared {
                client: reqwest::Client::new(),
                url: url.into(),
                credential,
                timeout: crate::stdio::DEFAULT_TIMEOUT,
                session_id: Mutex::new(None),
                sinks: Arc::new(NotificationSinks::new()),
                request_handler,
            }),
            next_id: AtomicI64::new(1),
            next_progress_token: AtomicI64::new(1),
            listener: Mutex::new(None),
        }
    }

    /// Connect (handshake only), for simple request/response use.
    pub async fn connect(
        url: impl Into<String>,
        credential: Credential,
    ) -> Result<Self, McpTransportError> {
        let transport = Self::new(url, credential);
        transport.initialize().await?;
        Ok(transport)
    }

    /// Connect and start the background GET SSE listener, so server->client
    /// streams (list_changed, resources/updated, sampling) are delivered.
    pub async fn connect_streaming(
        url: impl Into<String>,
        credential: Credential,
        sampling: Option<Arc<dyn crate::sampling::SamplingHandler>>,
    ) -> Result<Self, McpTransportError> {
        let handler = sampling.map(|s| {
            Arc::new(crate::sampling::SamplingBridge::new(s)) as Arc<dyn ServerRequestHandler>
        });
        let transport = Self::with_handler(url, credential, handler);
        transport.initialize().await?;
        transport.spawn_listener();
        Ok(transport)
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
        self.request("initialize", serde_json::to_value(&params)?)
            .await?;
        // `notifications/initialized` is a fire-and-forget notification.
        let _ = self
            .shared
            .post_builder(
                &json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }),
            )
            .send()
            .await;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpTransportError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = build_request_body(id, method, params);
        self.shared.request(&body, id).await
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
        let mut request = shared
            .client
            .get(&shared.url)
            .header("Accept", "text/event-stream");
        if let Some((name, value)) = shared.credential.header() {
            request = request.header(name, value);
        }
        if let Some(session) = shared.session_id.lock().unwrap().clone() {
            request = request.header(SESSION_HEADER, session);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                let mut stream = response.bytes_stream();
                let mut parser = SseParser::new();
                while let Some(Ok(chunk)) = stream.next().await {
                    let text = String::from_utf8_lossy(&chunk);
                    for data in parser.push(&text) {
                        if let Ok(value) = serde_json::from_str::<Value>(&data) {
                            shared.dispatch_incoming(value).await;
                        }
                    }
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
    events.extend(parser.push("\n\n"));
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
        let shared = Arc::new(HttpShared {
            client: reqwest::Client::new(),
            url: "http://unused".to_string(),
            credential: Credential::None,
            timeout: Duration::from_secs(1),
            session_id: Mutex::new(None),
            sinks: Arc::new(NotificationSinks::new()),
            request_handler: None,
        });
        let mut rx = shared.sinks.list_changed.subscribe();
        shared
            .dispatch_incoming(json!({
                "jsonrpc": "2.0", "method": "notifications/tools/list_changed"
            }))
            .await;
        assert_eq!(rx.recv().await.unwrap(), ListChangedKind::Tools);
    }
}
