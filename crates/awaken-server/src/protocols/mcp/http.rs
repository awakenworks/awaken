//! MCP HTTP transport: Axum routes for MCP Streamable HTTP.
//!
//! Implements the MCP Streamable HTTP transport specification:
//! - `POST /v1/mcp` — accepts a JSON-RPC request/notification/response.
//! - `GET  /v1/mcp` — optional SSE endpoint for server-initiated messages.
//! - `DELETE /v1/mcp` — optional explicit session termination.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::{HOST, ORIGIN};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures::StreamExt;
use mcp::protocol::{
    ClientInbound, JsonRpcId, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    ServerOutbound,
};
use mcp::server::McpServer;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock, Semaphore, mpsc, oneshot};
use uuid::Uuid;

use crate::app::AppState;
use crate::http_sse::{format_sse_data_with_id, sse_body_stream, sse_response};

const HEADER_SESSION_ID: &str = "MCP-Session-Id";
const HEADER_PROTOCOL_VERSION: &str = "MCP-Protocol-Version";
const JSON_RPC_VERSION: &str = "2.0";

#[derive(Default)]
pub struct McpHttpState {
    sessions: RwLock<HashMap<String, Arc<McpHttpSession>>>,
}

impl McpHttpState {
    pub fn new() -> Self {
        Self::default()
    }

    async fn insert(&self, session: Arc<McpHttpSession>) {
        self.sessions
            .write()
            .await
            .insert(session.id.clone(), session);
    }

    async fn get(&self, session_id: &str) -> Option<Arc<McpHttpSession>> {
        self.sessions.read().await.get(session_id).cloned()
    }

    async fn remove(&self, session_id: &str) -> Option<Arc<McpHttpSession>> {
        self.sessions.write().await.remove(session_id)
    }
}

struct ActiveSseStream {
    request_id: JsonRpcId,
    tx: mpsc::Sender<Bytes>,
}

struct McpHttpSession {
    id: String,
    server: Arc<McpServer>,
    inbound_tx: mpsc::Sender<ClientInbound>,
    protocol_version: RwLock<String>,
    pending_responses: Mutex<HashMap<JsonRpcId, oneshot::Sender<JsonRpcResponse>>>,
    active_stream: Mutex<Option<ActiveSseStream>>,
    request_permit: Arc<Semaphore>,
    next_event_id: AtomicU64,
}

impl McpHttpSession {
    fn new(runtime: &Arc<awaken_runtime::AgentRuntime>) -> Arc<Self> {
        let (server, mut channels) = super::create_mcp_server(runtime);
        let session = Arc::new(Self {
            id: Uuid::new_v4().to_string(),
            server,
            inbound_tx: channels.inbound_tx.clone(),
            protocol_version: RwLock::new(mcp::MCP_PROTOCOL_VERSION.to_string()),
            pending_responses: Mutex::new(HashMap::new()),
            active_stream: Mutex::new(None),
            request_permit: Arc::new(Semaphore::new(1)),
            next_event_id: AtomicU64::new(1),
        });

        let weak = Arc::downgrade(&session);
        tokio::spawn(async move {
            while let Some(outbound) = channels.outbound_rx.recv().await {
                let Some(session) = weak.upgrade() else {
                    break;
                };
                session.route_outbound(outbound).await;
            }
        });

        session
    }

    async fn stop(&self) {
        self.server.stop();
    }

    async fn set_protocol_version(&self, version: String) {
        *self.protocol_version.write().await = version;
    }

    async fn protocol_version(&self) -> String {
        self.protocol_version.read().await.clone()
    }

    async fn dispatch_json_request(
        &self,
        request: JsonRpcRequest,
    ) -> Result<JsonRpcResponse, McpApiError> {
        let _permit = self
            .request_permit
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| McpApiError::internal("failed to acquire MCP request permit"))?;

        let (tx, rx) = oneshot::channel();
        self.pending_responses
            .lock()
            .await
            .insert(request.id.clone(), tx);

        if let Err(_e) = self
            .inbound_tx
            .send(ClientInbound::Request(request.clone()))
            .await
        {
            self.pending_responses.lock().await.remove(&request.id);
            return Err(McpApiError::internal("server channel closed"));
        }

        rx.await
            .map_err(|_| McpApiError::internal("no response from MCP server"))
    }

    async fn dispatch_streaming_request(
        &self,
        request: JsonRpcRequest,
    ) -> Result<Response, McpApiError> {
        let permit = self
            .request_permit
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| McpApiError::internal("failed to acquire MCP request permit"))?;

        let (tx, rx) = mpsc::channel::<Bytes>(64);
        tx.send(self.prime_sse_frame())
            .await
            .map_err(|_| McpApiError::internal("failed to prime MCP SSE stream"))?;

        {
            let mut active = self.active_stream.lock().await;
            *active = Some(ActiveSseStream {
                request_id: request.id.clone(),
                tx: tx.clone(),
            });
        }

        if let Err(_e) = self.inbound_tx.send(ClientInbound::Request(request)).await {
            self.active_stream.lock().await.take();
            return Err(McpApiError::internal("server channel closed"));
        }

        let stream = sse_body_stream(rx);
        let guarded = async_stream::stream! {
            let _permit = permit;
            tokio::pin!(stream);
            while let Some(item) = stream.next().await {
                yield item;
            }
        };

        Ok(sse_response(guarded))
    }

    async fn send_inbound(&self, inbound: ClientInbound) -> Result<(), McpApiError> {
        self.inbound_tx
            .send(inbound)
            .await
            .map_err(|_| McpApiError::internal("server channel closed"))
    }

    async fn route_outbound(&self, outbound: ServerOutbound) {
        match outbound {
            ServerOutbound::Response(response) => {
                if self.route_response_to_stream(&response).await {
                    return;
                }
                if let Some(tx) = self.pending_responses.lock().await.remove(&response.id) {
                    let _ = tx.send(response);
                }
            }
            ServerOutbound::Notification(notification) => {
                self.route_stream_message(JsonRpcMessage::Notification(notification))
                    .await;
            }
            ServerOutbound::Request(request) => {
                self.route_stream_message(JsonRpcMessage::Request(request))
                    .await;
            }
        }
    }

    async fn route_response_to_stream(&self, response: &JsonRpcResponse) -> bool {
        let maybe_tx = {
            let mut active = self.active_stream.lock().await;
            if active
                .as_ref()
                .is_some_and(|stream| stream.request_id == response.id)
            {
                active.take().map(|stream| stream.tx)
            } else {
                None
            }
        };

        let Some(tx) = maybe_tx else {
            return false;
        };
        let Ok(json) = serde_json::to_string(response) else {
            return true;
        };
        let _ = tx.send(self.sse_data_frame(&json)).await;
        true
    }

    async fn route_stream_message(&self, message: JsonRpcMessage) {
        let tx = {
            self.active_stream
                .lock()
                .await
                .as_ref()
                .map(|stream| stream.tx.clone())
        };
        let Some(tx) = tx else {
            return;
        };
        let Ok(json) = serde_json::to_string(&message) else {
            return;
        };
        let _ = tx.send(self.sse_data_frame(&json)).await;
    }

    fn next_event_id(&self) -> u64 {
        self.next_event_id.fetch_add(1, Ordering::SeqCst)
    }

    fn prime_sse_frame(&self) -> Bytes {
        Bytes::from(format!("id: {}\ndata:\n\n", self.next_event_id()))
    }

    fn sse_data_frame(&self, json: &str) -> Bytes {
        format_sse_data_with_id(json, self.next_event_id())
    }
}

#[derive(Debug)]
enum ParsedInbound {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Response(JsonRpcResponse),
}

/// Build MCP routes.
pub fn mcp_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/mcp", post(mcp_post))
        .route("/v1/mcp", get(mcp_sse))
        .route("/v1/mcp", delete(mcp_delete))
}

async fn mcp_post(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, McpApiError> {
    validate_origin(&headers)?;
    validate_protocol_header_for_initialize(&headers)?;

    match parse_inbound_message(&body)? {
        ParsedInbound::Request(request) => handle_request_post(&st, headers, request).await,
        ParsedInbound::Notification(notification) => {
            let session = require_session(&st, &headers).await?;
            validate_session_protocol_header(&headers, &session).await?;
            session
                .send_inbound(ClientInbound::Notification(notification))
                .await?;
            Ok(StatusCode::ACCEPTED.into_response())
        }
        ParsedInbound::Response(response) => {
            let session = require_session(&st, &headers).await?;
            validate_session_protocol_header(&headers, &session).await?;
            session
                .send_inbound(ClientInbound::Response(response))
                .await?;
            Ok(StatusCode::ACCEPTED.into_response())
        }
    }
}

async fn handle_request_post(
    st: &AppState,
    headers: HeaderMap,
    request: JsonRpcRequest,
) -> Result<Response, McpApiError> {
    if request.method == "initialize" {
        if headers.contains_key(HEADER_SESSION_ID) {
            return Err(McpApiError::bad_request(
                "initialize requests must not include an MCP session id",
            ));
        }

        let session = McpHttpSession::new(&st.runtime);
        let response = session.dispatch_json_request(request).await?;
        if response.is_success() {
            if let Some(version) = response_protocol_version(&response) {
                session.set_protocol_version(version).await;
            }
            st.mcp_http.insert(Arc::clone(&session)).await;

            let mut http_response = Json(serde_json::to_value(&response).map_err(|e| {
                McpApiError::internal(format!("failed to serialize MCP response: {e}"))
            })?)
            .into_response();
            http_response.headers_mut().insert(
                HEADER_SESSION_ID,
                HeaderValue::from_str(&session.id).map_err(|e| {
                    McpApiError::internal(format!("invalid MCP session id header: {e}"))
                })?,
            );
            return Ok(http_response);
        }

        session.stop().await;
        return Ok(Json(serde_json::to_value(response).map_err(|e| {
            McpApiError::internal(format!("failed to serialize MCP response: {e}"))
        })?)
        .into_response());
    }

    let session = require_session(st, &headers).await?;
    validate_session_protocol_header(&headers, &session).await?;

    if request.method == "tools/call" {
        return session.dispatch_streaming_request(request).await;
    }

    let response = session.dispatch_json_request(request).await?;
    Ok(Json(
        serde_json::to_value(response)
            .map_err(|e| McpApiError::internal(format!("failed to serialize MCP response: {e}")))?,
    )
    .into_response())
}

async fn mcp_sse(State(_st): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(err) = validate_origin(&headers) {
        return err.into_response();
    }
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

async fn mcp_delete(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, McpApiError> {
    validate_origin(&headers)?;
    let session_id = header_value(&headers, HEADER_SESSION_ID)
        .ok_or_else(|| McpApiError::bad_request("missing MCP-Session-Id header"))?;

    let Some(session) = st.mcp_http.remove(&session_id).await else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    session.stop().await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn parse_inbound_message(body: &[u8]) -> Result<ParsedInbound, McpApiError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| McpApiError::parse_error(format!("failed to parse JSON-RPC message: {e}")))?;

    if !value.is_object() {
        return Err(McpApiError::invalid_request(
            None,
            "HTTP body must contain exactly one JSON-RPC object",
        ));
    }

    let id = parse_json_rpc_id(&value);
    validate_jsonrpc_version(&value, id.clone())?;

    let method = value.get("method").and_then(Value::as_str);
    if let Some(method) = method {
        if method.is_empty() {
            return Err(McpApiError::invalid_request(id, "missing 'method' field"));
        }

        match value.get("id") {
            Some(Value::Null) => {
                return Err(McpApiError::invalid_request(
                    Some(JsonRpcId::Null),
                    "MCP requests MUST use string or integer IDs; notifications MUST omit the id field",
                ));
            }
            Some(_) => {
                let request: JsonRpcRequest = serde_json::from_value(value).map_err(|e| {
                    McpApiError::invalid_request(id, format!("invalid JSON-RPC request: {e}"))
                })?;
                return Ok(ParsedInbound::Request(request));
            }
            None => {
                let notification: JsonRpcNotification =
                    serde_json::from_value(value).map_err(|e| {
                        McpApiError::invalid_request(
                            None,
                            format!("invalid JSON-RPC notification: {e}"),
                        )
                    })?;
                return Ok(ParsedInbound::Notification(notification));
            }
        }
    }

    if matches!(value.get("id"), Some(Value::Null)) {
        return Err(McpApiError::invalid_request(
            Some(JsonRpcId::Null),
            "JSON-RPC responses MUST use the original string or integer request id",
        ));
    }

    if value.get("result").is_some() || value.get("error").is_some() {
        let response: JsonRpcResponse = serde_json::from_value(value).map_err(|e| {
            McpApiError::invalid_request(id, format!("invalid JSON-RPC response: {e}"))
        })?;
        return Ok(ParsedInbound::Response(response));
    }

    Err(McpApiError::invalid_request(
        id,
        "unrecognized JSON-RPC payload",
    ))
}

fn validate_jsonrpc_version(value: &Value, id: Option<JsonRpcId>) -> Result<(), McpApiError> {
    match value.get("jsonrpc").and_then(Value::as_str) {
        Some(JSON_RPC_VERSION) => Ok(()),
        _ => Err(McpApiError::invalid_request(
            id,
            "JSON-RPC messages MUST include \"jsonrpc\": \"2.0\"",
        )),
    }
}

fn parse_json_rpc_id(value: &Value) -> Option<JsonRpcId> {
    value.get("id").map(|id| match id {
        Value::String(s) => JsonRpcId::String(s.clone()),
        Value::Number(n) => JsonRpcId::Number(n.as_i64().unwrap_or_default()),
        Value::Null => JsonRpcId::Null,
        _ => JsonRpcId::Null,
    })
}

async fn require_session(
    st: &AppState,
    headers: &HeaderMap,
) -> Result<Arc<McpHttpSession>, McpApiError> {
    let session_id = header_value(headers, HEADER_SESSION_ID)
        .ok_or_else(|| McpApiError::bad_request("missing MCP-Session-Id header"))?;
    st.mcp_http
        .get(&session_id)
        .await
        .ok_or_else(|| McpApiError::not_found("unknown MCP session"))
}

fn validate_origin(headers: &HeaderMap) -> Result<(), McpApiError> {
    let Some(origin_header) = headers.get(ORIGIN) else {
        return Ok(());
    };
    let origin = origin_header
        .to_str()
        .map_err(|_| McpApiError::forbidden("invalid Origin header"))?;
    let origin_uri: axum::http::Uri = origin
        .parse()
        .map_err(|_| McpApiError::forbidden("invalid Origin header"))?;
    let Some(origin_host) = origin_uri.host() else {
        return Err(McpApiError::forbidden("invalid Origin header"));
    };

    if is_loopback_host(origin_host) {
        return Ok(());
    }

    if let Some(host) = header_value(headers, HOST.as_str()) {
        let request_host = host.split(':').next().unwrap_or(host.as_str());
        if origin_host.eq_ignore_ascii_case(request_host) {
            return Ok(());
        }
    }

    Err(McpApiError::forbidden(
        "Origin header is not allowed for this MCP endpoint",
    ))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

fn validate_protocol_header_for_initialize(headers: &HeaderMap) -> Result<(), McpApiError> {
    let Some(version) = header_value(headers, HEADER_PROTOCOL_VERSION) else {
        return Ok(());
    };
    if version == mcp::MCP_PROTOCOL_VERSION {
        return Ok(());
    }
    Err(McpApiError::bad_request(format!(
        "unsupported MCP protocol version: {version}"
    )))
}

async fn validate_session_protocol_header(
    headers: &HeaderMap,
    session: &McpHttpSession,
) -> Result<(), McpApiError> {
    let Some(version) = header_value(headers, HEADER_PROTOCOL_VERSION) else {
        return Ok(());
    };
    let expected = session.protocol_version().await;
    if version == expected {
        return Ok(());
    }
    Err(McpApiError::bad_request(format!(
        "unsupported MCP protocol version: {version}"
    )))
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn response_protocol_version(response: &JsonRpcResponse) -> Option<String> {
    response
        .result()
        .and_then(|result| result.get("protocolVersion"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

#[derive(Debug)]
pub struct McpApiError {
    status: StatusCode,
    code: i64,
    message: String,
    id: Option<JsonRpcId>,
}

impl McpApiError {
    fn parse_error(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: -32700,
            message: message.into(),
            id: None,
        }
    }

    fn invalid_request(id: Option<JsonRpcId>, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: -32600,
            message: message.into(),
            id,
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::invalid_request(None, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: -32600,
            message: message.into(),
            id: None,
        }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: -32600,
            message: message.into(),
            id: None,
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: -32603,
            message: message.into(),
            id: None,
        }
    }
}

impl IntoResponse for McpApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({
                "jsonrpc": JSON_RPC_VERSION,
                "error": {
                    "code": self.code,
                    "message": self.message,
                },
                "id": self.id.map(|id| serde_json::to_value(id).unwrap_or(Value::Null)).unwrap_or(Value::Null),
            })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime::{AgentResolver, AgentRuntime, ResolvedAgent, RuntimeError};
    use awaken_stores::InMemoryMailboxStore;
    use awaken_stores::memory::InMemoryStore;
    use serde_json::json;

    struct StubResolver;
    impl AgentResolver for StubResolver {
        fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
            Err(RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
        fn agent_ids(&self) -> Vec<String> {
            vec!["echo-agent".into()]
        }
    }

    fn make_app_state() -> AppState {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
        let store = Arc::new(InMemoryStore::new());
        let mailbox_store = Arc::new(InMemoryMailboxStore::new());
        let mailbox = Arc::new(crate::mailbox::Mailbox::new(
            Arc::clone(&runtime),
            mailbox_store,
            "test".to_string(),
            crate::mailbox::MailboxConfig::default(),
        ));
        AppState::new(
            runtime,
            mailbox,
            store,
            Arc::new(StubResolver),
            crate::app::ServerConfig::default(),
        )
    }

    fn extract_session_id(response: &Response) -> String {
        response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .expect("missing session id")
            .to_string()
    }

    async fn call(app: axum::Router, request: axum::http::Request<axum::body::Body>) -> Response {
        tower::ServiceExt::oneshot(app, request).await.unwrap()
    }

    #[tokio::test]
    async fn post_initialize_assigns_session_id() {
        let app = Router::new()
            .merge(mcp_routes())
            .with_state(make_app_state());

        let body = json!({
            "jsonrpc": JSON_RPC_VERSION,
            "method": "initialize",
            "params": {
                "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0.0"}
            },
            "id": 1
        });

        let response = call(
            app,
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/mcp")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(HEADER_SESSION_ID));
    }

    #[tokio::test]
    async fn post_requires_session_after_initialize() {
        let app = Router::new()
            .merge(mcp_routes())
            .with_state(make_app_state());

        let body = json!({
            "jsonrpc": JSON_RPC_VERSION,
            "method": "tools/list",
            "id": 2
        });

        let response = call(
            app,
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/mcp")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_terminates_known_session() {
        let app = Router::new()
            .merge(mcp_routes())
            .with_state(make_app_state());

        let init = json!({
            "jsonrpc": JSON_RPC_VERSION,
            "method": "initialize",
            "params": {
                "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0.0"}
            },
            "id": 1
        });

        let init_response = call(
            app.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/mcp")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&init).unwrap()))
                .unwrap(),
        )
        .await;
        let session_id = extract_session_id(&init_response);

        let delete_response = call(
            app,
            axum::http::Request::builder()
                .method("DELETE")
                .uri("/v1/mcp")
                .header(HEADER_SESSION_ID, session_id)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(delete_response.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn parse_inbound_rejects_id_null_request() {
        let body = serde_json::to_vec(&json!({
            "jsonrpc": JSON_RPC_VERSION,
            "method": "tools/list",
            "id": null
        }))
        .unwrap();
        let err = parse_inbound_message(&body).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.code, -32600);
    }

    #[test]
    fn validate_origin_accepts_loopback() {
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_static("http://127.0.0.1:3000"));
        assert!(validate_origin(&headers).is_ok());
    }

    #[test]
    fn validate_origin_rejects_remote_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_static("https://evil.example"));
        headers.insert(HOST, HeaderValue::from_static("127.0.0.1:3000"));
        let err = validate_origin(&headers).unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn sse_prime_frame_contains_empty_data_event() {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
        let session = McpHttpSession::new(&runtime);
        let frame = session.prime_sse_frame();
        let text = std::str::from_utf8(&frame).unwrap();
        assert!(text.contains("data:\n\n"));
    }
}
