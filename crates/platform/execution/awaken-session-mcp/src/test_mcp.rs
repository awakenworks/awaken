// @formal surface exclude=test_only
// @formal owner crates/platform/execution/awaken-session-mcp/src/mcp.rs
// @formal reason: In-process MCP protocol emulator declared under cfg(test) or the test-support feature
// @formal reason: and used only by session-mcp and runtime-host tests.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use awaken_sync::LeafLock;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

pub type SeenMcpRequests = Arc<LeafLock<Vec<(String, String)>>>;

/// Pause the real initialize response at a controlled preparation fault cut.
pub struct InitializeGate {
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}

impl Default for InitializeGate {
    fn default() -> Self {
        Self::new()
    }
}

impl InitializeGate {
    pub fn new() -> Self {
        Self {
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        }
    }

    pub(crate) async fn pause(&self) {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
    }

    pub async fn entered(&self) {
        self.entered.acquire().await.unwrap().forget();
    }

    pub fn release(&self) {
        self.release.add_permits(1);
    }
}

pub async fn start_with_initialize_gate(gate: Arc<InitializeGate>) -> (String, SeenMcpRequests) {
    start_with_gate(None, false, Duration::ZERO, false, Some(gate)).await
}

/// One shared Streamable-HTTP MCP fixture for Host and relay tests. Keeping the
/// protocol emulator here prevents each security test from growing a subtly
/// different initialize/list/call implementation.
pub async fn start(required_bearer: Option<&str>) -> (String, SeenMcpRequests) {
    start_with_options(required_bearer, false, Duration::ZERO, false).await
}

pub async fn start_stateful(required_bearer: Option<&str>) -> (String, SeenMcpRequests) {
    start_with_options(required_bearer, false, Duration::ZERO, true).await
}

pub async fn start_with_prompts(
    required_bearer: Option<&str>,
    prompts: bool,
) -> (String, SeenMcpRequests) {
    start_with_options(required_bearer, prompts, Duration::ZERO, false).await
}

pub async fn start_with_tool_call_delay(delay: Duration) -> (String, SeenMcpRequests) {
    start_with_options(None, false, delay, false).await
}

#[derive(Clone)]
struct FixtureState {
    seen: SeenMcpRequests,
    required_bearer: Option<String>,
    prompts: bool,
    tool_call_delay: Duration,
    stateful: bool,
    session_live: Arc<AtomicBool>,
    initialize_gate: Option<Arc<InitializeGate>>,
}

async fn start_with_options(
    required_bearer: Option<&str>,
    prompts: bool,
    tool_call_delay: Duration,
    stateful: bool,
) -> (String, SeenMcpRequests) {
    start_with_gate(required_bearer, prompts, tool_call_delay, stateful, None).await
}

async fn mcp(State(state): State<FixtureState>, req: Request) -> Response {
    use awaken_mcp_wire::http::{MCP_PROTOCOL_VERSION_HEADER, MCP_SESSION_ID_HEADER};
    const SESSION: &str = "host-test-session";
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<none>")
        .to_string();
    if state
        .required_bearer
        .as_deref()
        .is_some_and(|required| required != bearer)
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let has_session = req
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .is_some_and(|value| value == SESSION)
        && state.session_live.load(Ordering::Acquire);
    if req.method() != axum::http::Method::POST {
        if !state.stateful {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        if !has_session {
            return StatusCode::BAD_REQUEST.into_response();
        }
        state.seen.lock().push((req.method().to_string(), bearer));
        return match *req.method() {
            axum::http::Method::GET => ([(header::CONTENT_TYPE, "text/event-stream")],
                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n").into_response(),
            axum::http::Method::DELETE => {
                state.session_live.store(false, Ordering::Release);
                StatusCode::NO_CONTENT.into_response()
            }
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        };
    }
    let body = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
        }
    };
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<missing>")
        .to_string();
    if state.stateful && method != "initialize" && !has_session {
        return StatusCode::BAD_REQUEST.into_response();
    }
    state.seen.lock().push((method.clone(), bearer));
    if method == "initialize"
        && let Some(gate) = &state.initialize_gate
    {
        gate.pause().await;
    }
    let Some(id) = value.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match method.as_str() {
        "initialize" => state.initialize(),
        "tools/list" => serde_json::json!({
            "tools": [{
                "name": "echo",
                "description": "echo one value",
                "inputSchema": {
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "required": ["value"]
                }
            }]
        }),
        "tools/call" => {
            tokio::time::sleep(state.tool_call_delay).await;
            serde_json::json!({
                "content": [{ "type": "text", "text": value["params"]["arguments"]["value"] }],
                "isError": false
            })
        }
        "prompts/list" if state.prompts => serde_json::json!({
            "prompts": [{
                "name": "review",
                "title": "Review",
                "description": "Review with a named focus",
                "arguments": [{ "name": "focus", "required": true }]
            }]
        }),
        "prompts/get" if state.prompts => serde_json::json!({
            "description": "Rendered review instructions",
            "messages": [{
                "role": "user",
                "content": {
                    "type": "text",
                    "text": format!(
                        "Review focus: {}",
                        value["params"]["arguments"]["focus"]
                            .as_str()
                            .unwrap_or_default()
                    )
                }
            }]
        }),
        _ => {
            return axum::Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "method not found" }
            }))
            .into_response();
        }
    };
    let mut response = axum::Json(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    }))
    .into_response();
    if state.stateful && method == "initialize" {
        response
            .headers_mut()
            .insert(MCP_SESSION_ID_HEADER, SESSION.parse().unwrap());
        response
            .headers_mut()
            .insert(MCP_PROTOCOL_VERSION_HEADER, "2025-06-18".parse().unwrap());
        // These transport/credential fields must never escape the relay.
        response
            .headers_mut()
            .insert("set-cookie", "secret=forbidden".parse().unwrap());
    }
    response
}

impl FixtureState {
    fn initialize(&self) -> serde_json::Value {
        self.session_live.store(true, Ordering::Release);
        let capabilities = if self.prompts {
            serde_json::json!({ "tools": {}, "prompts": {} })
        } else {
            serde_json::json!({ "tools": {} })
        };
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": capabilities,
            "serverInfo": { "name": "host-test", "version": "1" }
        })
    }
}

async fn start_with_gate(
    required_bearer: Option<&str>,
    prompts: bool,
    tool_call_delay: Duration,
    stateful: bool,
    initialize_gate: Option<Arc<InitializeGate>>,
) -> (String, SeenMcpRequests) {
    let seen = SeenMcpRequests::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/mcp", axum::routing::any(mcp))
        .with_state(FixtureState {
            seen: seen.clone(),
            required_bearer: required_bearer.map(str::to_string),
            prompts,
            tool_call_delay,
            stateful,
            session_live: Arc::new(AtomicBool::new(false)),
            initialize_gate,
        });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/mcp"), seen)
}

/// The generation fence of one bare `https://fixture.test/mcp` attachment,
/// admitted through the fence's one persisted-coordinate door.
pub fn mcp_fence(
    session: impl Into<awaken_session_types::SessionId>,
    attachment: impl Into<String>,
    version: u64,
    incarnation: impl Into<String>,
    epoch: u64,
    expires_at_unix_ms: u64,
) -> awaken_session_configuration_contract::mcp::McpGenerationRef {
    awaken_session_configuration_contract::mcp::McpGenerationRef::from_persisted(
        session.into(),
        awaken_session_configuration_contract::mcp::McpGeneration::of_bare_target(
            awaken_session_configuration_contract::mcp::McpAttachmentId(attachment.into()),
            version,
            &awaken_agent_contract::McpTarget::parse_http("https://fixture.test/mcp").unwrap(),
        ),
        incarnation.into(),
        epoch,
        expires_at_unix_ms,
    )
    .expect("a fixture fence epoch is nonzero")
}
