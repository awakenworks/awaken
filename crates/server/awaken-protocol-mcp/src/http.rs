//! Streamable HTTP MCP server (axum router).
//!
//! The inverse of the client transport in `awaken-ext-mcp/src/http.rs`, and
//! shaped to interoperate with it:
//!
//! - **POST** — one JSON-RPC message per request. A `tools/call` carrying a
//!   `progressToken` answers with an SSE stream (`notifications/progress`
//!   events, then the final response); everything else answers plain JSON.
//!   Notifications (`notifications/initialized`) answer `202 Accepted`.
//! - **GET** — a standing SSE stream for server->client notifications
//!   (`notifications/tools/list_changed` when the export set changes).
//! - **DELETE** — ends the session.
//! - `initialize` opens a session echoed via the `Mcp-Session-Id` header; a
//!   request naming an unknown session is `404` (the client then re-initializes).
//! - An optional bearer token guards every route; a mismatch is `401` with a
//!   `WWW-Authenticate` challenge, which the client's credential-refresh path
//!   consumes.
//!
//! Mount the router alongside the other protocol routers (A2A, AG-UI, …).

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};

use crate::service::{McpToolService, NotifySink, NullSink};

/// The session header of the Streamable HTTP transport.
const SESSION_HEADER: &str = "Mcp-Session-Id";

/// Configuration for one mounted MCP endpoint.
pub struct McpHttpConfig {
    /// The endpoint path (a Streamable HTTP client POSTs, GETs, and DELETEs
    /// one URL).
    pub path: String,
    /// When set, every request must carry `Authorization: Bearer <token>`;
    /// a mismatch is a 401 challenge.
    pub bearer_token: Option<String>,
}

impl Default for McpHttpConfig {
    fn default() -> Self {
        Self {
            path: "/mcp".to_string(),
            bearer_token: None,
        }
    }
}

struct HttpState {
    service: Arc<McpToolService>,
    bearer_token: Option<String>,
    /// Sessions opened by `initialize` and not yet DELETEd.
    sessions: RwLock<HashSet<String>>,
    /// Server->client notifications fanned out to every standing GET stream.
    notifications: broadcast::Sender<Value>,
}

/// Build the MCP router over `service`. The `tools/list_changed` pump runs for
/// the router's lifetime when the service's export source is watchable.
pub fn router(service: Arc<McpToolService>, config: McpHttpConfig) -> Router {
    let state = Arc::new(HttpState {
        bearer_token: config.bearer_token,
        sessions: RwLock::new(HashSet::new()),
        notifications: broadcast::channel(64).0,
        service: Arc::clone(&service),
    });

    if let Some(mut changes) = service.source().changes() {
        let notifications = state.notifications.clone();
        tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                let _ =
                    notifications.send(awaken_mcp_server_core::tools_list_changed_notification());
            }
        });
    }

    Router::new()
        .route(
            &config.path,
            post(handle_post).get(handle_get).delete(handle_delete),
        )
        .with_state(state)
}

/// Pushes notifications as SSE events onto one in-flight POST response.
struct SseResponseSink {
    events: mpsc::Sender<Value>,
}

#[async_trait]
impl NotifySink for SseResponseSink {
    async fn notify(&self, method: &str, params: Value) {
        let _ = self
            .events
            .send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await;
    }
}

async fn handle_post(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Some(challenge) = unauthorized(&state, &headers) {
        return challenge;
    }
    if let Some(rejection) = core_preflight(awaken_mcp_server_core::McpHttpMethod::Post, &headers) {
        return rejection;
    }
    let Ok(message) = serde_json::from_str::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "invalid JSON").into_response();
    };
    if let Some(rejection) =
        awaken_mcp_server_core::validate_streamable_http_message(&headers, &message)
    {
        return core_reply(rejection);
    }
    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id").filter(|value| !value.is_null()).cloned();
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    if let (Some(method), Some(id)) = (method, id.as_ref()) {
        if method != "initialize" {
            // A stale session (server restarted, or DELETEd) is 404: the
            // client reacts by re-initializing, per the Streamable HTTP spec.
            if let Some(session) = header_value(&headers, SESSION_HEADER)
                && !state
                    .sessions
                    .read()
                    .expect("session lock")
                    .contains(&session)
            {
                return (StatusCode::NOT_FOUND, "unknown session").into_response();
            }
        }
        let wants_progress = method == "tools/call"
            && params
                .get("_meta")
                .and_then(|meta| meta.get("progressToken"))
                .is_some_and(|token| !token.is_null());
        if wants_progress {
            return progress_call_response(&state, id.clone(), method.to_string(), params);
        }
    }

    let is_initialize = method == Some("initialize") && id.is_some();
    let reply = state
        .service
        .handle_http(&headers, body.as_bytes(), &NullSink)
        .await;
    let successful_initialize = is_initialize
        && matches!(
            &reply.body,
            awaken_mcp_server_core::McpHttpBody::Json(value) if value.get("result").is_some()
        );
    let mut response = core_reply(reply);
    if successful_initialize {
        let session = uuid::Uuid::new_v4().simple().to_string();
        state
            .sessions
            .write()
            .expect("session lock")
            .insert(session.clone());
        response.headers_mut().insert(
            SESSION_HEADER,
            session
                .parse()
                .expect("generated session id is a header value"),
        );
    }
    response
}

/// Answer a progress-tracked `tools/call` with an SSE stream: each progress
/// notification is one event, and the final JSON-RPC response ends the stream.
fn progress_call_response(
    state: &Arc<HttpState>,
    id: Value,
    method: String,
    params: Value,
) -> Response {
    let (events_tx, events_rx) = mpsc::channel::<Value>(64);
    let service = Arc::clone(&state.service);
    tokio::spawn(async move {
        let sink = Arc::new(SseResponseSink {
            events: events_tx.clone(),
        });
        let reply = awaken_mcp_server_core::jsonrpc_reply(
            id.clone(),
            service
                .handle_request(&id, &method, params, sink.as_ref())
                .await,
        );
        let _ = events_tx.send(reply).await;
    });
    let stream = ReceiverStream::new(events_rx)
        .map(|message| Ok::<_, Infallible>(Event::default().data(message.to_string())));
    Sse::new(stream).into_response()
}

/// The standing server->client stream: every broadcast notification becomes an
/// SSE event; lagged subscribers just skip (notifications are refresh hints,
/// not state).
async fn handle_get(State(state): State<Arc<HttpState>>, headers: HeaderMap) -> Response {
    if let Some(challenge) = unauthorized(&state, &headers) {
        return challenge;
    }
    if let Some(rejection) = core_preflight(awaken_mcp_server_core::McpHttpMethod::Get, &headers) {
        return rejection;
    }
    let stream = BroadcastStream::new(state.notifications.subscribe()).filter_map(|message| {
        message
            .ok()
            .map(|value| Ok::<_, Infallible>(Event::default().data(value.to_string())))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn handle_delete(State(state): State<Arc<HttpState>>, headers: HeaderMap) -> Response {
    if let Some(challenge) = unauthorized(&state, &headers) {
        return challenge;
    }
    if let Some(rejection) = core_preflight(awaken_mcp_server_core::McpHttpMethod::Delete, &headers)
    {
        return rejection;
    }
    if let Some(session) = header_value(&headers, SESSION_HEADER) {
        state
            .sessions
            .write()
            .expect("session lock")
            .remove(&session);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `Some(challenge)` when a configured bearer token is missing or wrong. The
/// `WWW-Authenticate` header feeds the client's credential-refresh path.
fn unauthorized(state: &HttpState, headers: &HeaderMap) -> Option<Response> {
    let expected = state.bearer_token.as_ref()?;
    let presented = header_value(headers, header::AUTHORIZATION.as_str());
    if presented.as_deref() == Some(&format!("Bearer {expected}")) {
        return None;
    }
    Some(
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"mcp\"")],
            "unauthorized",
        )
            .into_response(),
    )
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn core_preflight(
    method: awaken_mcp_server_core::McpHttpMethod,
    headers: &HeaderMap,
) -> Option<Response> {
    awaken_mcp_server_core::validate_streamable_http_request(
        method,
        headers,
        &awaken_mcp_server_core::AllowAllOrigins,
    )
    .map(core_reply)
}

fn core_reply(reply: awaken_mcp_server_core::McpHttpReply) -> Response {
    let mut response = match reply.body {
        awaken_mcp_server_core::McpHttpBody::Empty => reply.status.into_response(),
        awaken_mcp_server_core::McpHttpBody::Text(text) => (reply.status, text).into_response(),
        awaken_mcp_server_core::McpHttpBody::Json(value) => {
            (reply.status, axum::Json(value)).into_response()
        }
        awaken_mcp_server_core::McpHttpBody::EventStream => {
            unreachable!("standing SSE responses are realized by handle_get")
        }
        awaken_mcp_server_core::McpHttpBody::Sse(events) => {
            let stream =
                tokio_stream::iter(events.into_iter().map(|message| {
                    Ok::<_, Infallible>(Event::default().data(message.to_string()))
                }));
            Sse::new(stream).into_response()
        }
    };
    response.headers_mut().extend(reply.headers);
    response
}
