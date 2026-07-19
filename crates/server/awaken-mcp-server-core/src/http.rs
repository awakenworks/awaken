//! Axum-free Streamable HTTP decision kernel.

use std::sync::Mutex;

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use serde_json::Value;

use crate::{McpServer, McpToolHost, NotifySink, jsonrpc_reply};

pub const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

/// Authentication/business-context construction port for framework wrappers.
/// The protocol core never interprets the resulting context.
#[async_trait]
pub trait McpHttpContextProvider<C>: Send + Sync {
    async fn context(&self, parts: &http::request::Parts) -> Result<C, HttpContextError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpContextError {
    pub message: String,
}

impl HttpContextError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HttpContextError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HttpContextError {}

/// The transport verb understood by the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpHttpMethod {
    Get,
    Post,
    Delete,
    Other,
}

/// Transport-neutral reply body. Axum/Hyper wrappers only map this value onto
/// their concrete response body.
#[derive(Debug, Clone, PartialEq)]
pub enum McpHttpBody {
    Empty,
    Text(String),
    Json(Value),
    /// Ordered SSE event payloads: zero or more notifications followed by the
    /// single final JSON-RPC response.
    Sse(Vec<Value>),
}

/// Complete axum-free HTTP response decision.
#[derive(Debug, Clone, PartialEq)]
pub struct McpHttpReply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: McpHttpBody,
}

impl McpHttpReply {
    fn new(status: StatusCode, body: McpHttpBody) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body,
        }
    }
}

/// Caller-supplied origin policy. Authentication and context construction stay
/// outside the protocol core.
pub trait OriginPolicy: Send + Sync {
    fn allows(&self, origin: Option<&str>) -> bool;
}

/// Explicit permissive policy for trusted/local wrappers.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAllOrigins;

impl OriginPolicy for AllowAllOrigins {
    fn allows(&self, _origin: Option<&str>) -> bool {
        true
    }
}

impl<F> OriginPolicy for F
where
    F: for<'a> Fn(Option<&'a str>) -> bool + Send + Sync,
{
    fn allows(&self, origin: Option<&str>) -> bool {
        self(origin)
    }
}

/// Apply Streamable HTTP semantics without depending on axum. Notifications
/// return 202 with no JSON-RPC body; requests return one JSON envelope. Streaming
/// progress remains a transport wrapper concern because it requires a concrete
/// asynchronous response body, but dispatch and final-envelope semantics are
/// shared here.
pub async fn handle_streamable_http<H, C>(
    server: &McpServer<H, C>,
    context: &C,
    method: McpHttpMethod,
    headers: &HeaderMap,
    body: &[u8],
    origin_policy: &dyn OriginPolicy,
    notifications: &dyn NotifySink,
) -> McpHttpReply
where
    H: McpToolHost<C>,
    C: Send + Sync,
{
    if let Some(reply) = validate_streamable_http_request(method, headers, origin_policy) {
        return reply;
    }
    match method {
        McpHttpMethod::Get => McpHttpReply::new(StatusCode::OK, McpHttpBody::Empty),
        McpHttpMethod::Delete => McpHttpReply::new(StatusCode::NO_CONTENT, McpHttpBody::Empty),
        McpHttpMethod::Other => unreachable!("preflight rejects unsupported methods"),
        McpHttpMethod::Post => handle_post(server, context, headers, body, notifications).await,
    }
}

/// Shared transport preflight used by both the axum-free kernel and wrappers
/// that provide a concrete asynchronous SSE body. Returning the rejection reply
/// keeps Accept/Origin/method semantics identical on both paths.
pub fn validate_streamable_http_request(
    method: McpHttpMethod,
    headers: &HeaderMap,
    origin_policy: &dyn OriginPolicy,
) -> Option<McpHttpReply> {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if !origin_policy.allows(origin) {
        return Some(McpHttpReply::new(
            StatusCode::FORBIDDEN,
            McpHttpBody::Text("origin denied".into()),
        ));
    }
    match method {
        McpHttpMethod::Get if !accepts(headers, "text/event-stream") => Some(McpHttpReply::new(
            StatusCode::METHOD_NOT_ALLOWED,
            McpHttpBody::Empty,
        )),
        McpHttpMethod::Post
            if headers.contains_key(header::ACCEPT)
                && !accepts(headers, "application/json")
                && !accepts(headers, "text/event-stream")
                && !accepts(headers, "*/*") =>
        {
            Some(McpHttpReply::new(
                StatusCode::NOT_ACCEPTABLE,
                McpHttpBody::Text("unsupported Accept".into()),
            ))
        }
        McpHttpMethod::Other => Some(McpHttpReply::new(
            StatusCode::METHOD_NOT_ALLOWED,
            McpHttpBody::Empty,
        )),
        McpHttpMethod::Get | McpHttpMethod::Post | McpHttpMethod::Delete => None,
    }
}

/// Validate the per-request protocol version before entering a business host.
pub fn validate_protocol_version(headers: &HeaderMap, initializing: bool) -> Option<McpHttpReply> {
    if !initializing
        && let Some(version) = headers
            .get(MCP_PROTOCOL_VERSION_HEADER)
            .and_then(|value| value.to_str().ok())
        && !crate::SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
    {
        return Some(McpHttpReply::new(
            StatusCode::BAD_REQUEST,
            McpHttpBody::Text(format!("unsupported MCP protocol version: {version}")),
        ));
    }
    None
}

async fn handle_post<H, C>(
    server: &McpServer<H, C>,
    context: &C,
    headers: &HeaderMap,
    body: &[u8],
    notifications: &dyn NotifySink,
) -> McpHttpReply
where
    H: McpToolHost<C>,
    C: Send + Sync,
{
    let message: Value = match serde_json::from_slice(body) {
        Ok(message) => message,
        Err(_) => {
            return McpHttpReply::new(
                StatusCode::BAD_REQUEST,
                McpHttpBody::Text("invalid JSON".into()),
            );
        }
    };
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return McpHttpReply::new(
            StatusCode::BAD_REQUEST,
            McpHttpBody::Text("not a JSON-RPC message".into()),
        );
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let id = message.get("id").filter(|id| !id.is_null()).cloned();
    let Some(id) = id else {
        server.handle_notification(context, method, &params).await;
        return McpHttpReply::new(StatusCode::ACCEPTED, McpHttpBody::Empty);
    };

    if validate_protocol_version(headers, method == "initialize").is_some() {
        let version = headers
            .get(MCP_PROTOCOL_VERSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("invalid");
        let outcome = Err(
            awaken_mcp_wire::jsonrpc::ServerRequestError::invalid_params(format!(
                "unsupported MCP protocol version: {version}"
            )),
        );
        return json_reply(id, outcome);
    }

    let wants_progress = method == "tools/call"
        && params
            .get("_meta")
            .and_then(|meta| meta.get("progressToken"))
            .is_some_and(|token| !token.is_null());
    if wants_progress {
        let sink = CollectSink::default();
        let outcome = server
            .handle_request(context, Some(&id), method, params, &sink)
            .await;
        let mut events = sink.into_events();
        events.push(jsonrpc_reply(id, outcome));
        return McpHttpReply::new(StatusCode::OK, McpHttpBody::Sse(events));
    }
    let outcome = server
        .handle_request(context, Some(&id), method, params, notifications)
        .await;
    let mut reply = json_reply(id, outcome);
    if method == "initialize" {
        reply.headers.insert(
            HeaderName::from_static(MCP_PROTOCOL_VERSION_HEADER),
            HeaderValue::from_static(mcp::MCP_PROTOCOL_VERSION),
        );
    }
    reply
}

#[derive(Default)]
struct CollectSink {
    events: Mutex<Vec<Value>>,
}

impl CollectSink {
    fn into_events(self) -> Vec<Value> {
        self.events.into_inner().expect("MCP SSE event collector")
    }
}

#[async_trait]
impl NotifySink for CollectSink {
    async fn notify(&self, method: &str, params: Value) {
        self.events
            .lock()
            .expect("MCP SSE event collector")
            .push(serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }));
    }
}

fn json_reply(
    id: Value,
    outcome: Result<Value, awaken_mcp_wire::jsonrpc::ServerRequestError>,
) -> McpHttpReply {
    McpHttpReply::new(
        StatusCode::OK,
        McpHttpBody::Json(jsonrpc_reply(id, outcome)),
    )
}

fn accepts(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|value| value.split(';').next() == Some(expected))
}
