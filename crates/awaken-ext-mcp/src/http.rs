//! HTTP (Streamable) MCP transport.
//!
//! POSTs JSON-RPC requests to an MCP endpoint and parses the response, which may
//! be `application/json` or an SSE `text/event-stream`. Like the stdio transport
//! it keeps the raw [`CallToolResult`] (and its `isError` flag) so the
//! three-state mapping in [`McpRawTool`](crate::tool::McpRawTool) survives. A
//! [`Credential`] adds an opaque auth header, and the server's session id (if
//! any) is echoed on subsequent requests.
//!
//! Server->client streams (progress/list_changed/sampling) over HTTP need a
//! long-lived SSE channel and are not part of this transport yet; those flow
//! over stdio today.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mcp::transport::McpTransportError;
use mcp::{CallToolParams, CallToolResult, InitializeParams, ListToolsResult, McpToolDefinition};
use serde_json::{Value, json};

use crate::credential::Credential;
use crate::transport::McpToolTransport;
use crate::types::{
    ListPromptsResult, ListResourcesResult, McpPromptDefinition, McpPromptResult,
    McpResourceDefinition,
};

const SESSION_HEADER: &str = "Mcp-Session-Id";

/// An HTTP MCP server connection presented as an [`McpToolTransport`].
pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    credential: Credential,
    timeout: Duration,
    next_id: AtomicI64,
    session_id: Mutex<Option<String>>,
}

impl HttpTransport {
    /// Build a transport for `url`, authenticating with `credential`.
    pub fn new(url: impl Into<String>, credential: Credential) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.into(),
            credential,
            timeout: crate::stdio::DEFAULT_TIMEOUT,
            next_id: AtomicI64::new(1),
            session_id: Mutex::new(None),
        }
    }

    /// Set the per-request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Complete the MCP handshake, capturing any session id the server assigns.
    pub async fn connect(
        url: impl Into<String>,
        credential: Credential,
    ) -> Result<Self, McpTransportError> {
        let transport = Self::new(url, credential);
        transport.initialize().await?;
        Ok(transport)
    }

    async fn initialize(&self) -> Result<(), McpTransportError> {
        let params = InitializeParams::new(None);
        self.request("initialize", serde_json::to_value(&params)?)
            .await?;
        // `notifications/initialized` is a fire-and-forget notification.
        let _ = self
            .post(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }))
            .await;
        Ok(())
    }

    /// Issue a JSON-RPC request and return its `result`.
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpTransportError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = build_request_body(id, method, params);
        let text = self.post(&body).await?;
        let value = parse_sse_or_json(&text)?;
        extract_result(value)
    }

    /// POST a JSON body and return the response text, applying auth + session
    /// headers and capturing any session id the server returns.
    async fn post(&self, body: &Value) -> Result<String, McpTransportError> {
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
        let response = request
            .send()
            .await
            .map_err(|e| McpTransportError::TransportError(e.to_string()))?;

        if let Some(session) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().unwrap() = Some(session.to_string());
        }

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| McpTransportError::TransportError(e.to_string()))?;
        if !status.is_success() {
            return Err(McpTransportError::ServerError(format!(
                "HTTP {status}: {text}"
            )));
        }
        Ok(text)
    }
}

/// Build a JSON-RPC request body.
fn build_request_body(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// Parse a response body that is either `application/json` or SSE
/// `text/event-stream`, returning the JSON-RPC message.
fn parse_sse_or_json(text: &str) -> Result<Value, McpTransportError> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return serde_json::from_str(trimmed)
            .map_err(|e| McpTransportError::ProtocolError(e.to_string()));
    }
    // SSE: return the last `data:` payload that parses as JSON.
    let mut last: Option<Value> = None;
    for line in text.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            if let Ok(value) = serde_json::from_str::<Value>(payload.trim()) {
                last = Some(value);
            }
        }
    }
    last.ok_or_else(|| McpTransportError::ProtocolError("no JSON in SSE response".to_string()))
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
}
