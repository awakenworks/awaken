//! End-to-end MCP over the management server (ADR-0043 Phase 3): a managed
//! session's MCP servers — bound either inline (session `mcp_servers` +
//! `vault_ids`) or through the management plane's agent↔MCP config — are
//! connected over real Streamable HTTP via `awaken-ext-mcp`, and the agent
//! calls the server's tool with the injected vault credential across a
//! multi-turn conversation.
//!
//! The in-process mock MCP server pins the fixture contract a Node e2e fixture
//! must mirror (JSON-RPC 2.0 over POST):
//! - `initialize` → any result (the client discards it), then the client fires
//!   the `notifications/initialized` notification (no `id`; answer 202);
//! - `tools/list` → `{ "tools": [{ "name": "add", "inputSchema": {...} }] }`;
//! - `tools/call` (`params.name = "add"`, `params.arguments = {a, b}`) →
//!   `{ "content": [{ "type": "text", "text": "<a+b>" }], "isError": false }`;
//! - EVERY request must carry `Authorization: Bearer <token>`, else 401 — the
//!   handshake fails and the turn fails loudly (never a silent skip).
//!
//! The server registers as name `calc`, so the runtime tool id is
//! `mcp__calc__add` (`awaken_ext_mcp::to_tool_id`), which is exactly what the
//! deterministic `McpToolModel` calls on `add <a> <b>`.

use awaken_server_local::build_management_router;
use axum::Router;
use axum::body::Body;
use axum::extract::Json;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const CALC_TOKEN: &str = "calc-bearer-token"; // awaken-allow: secret

/// The JSON-RPC endpoint of the mock `calc` MCP server.
async fn rpc(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let expected = format!("Bearer {CALC_TOKEN}");
    let authorized = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == expected);
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let id = body["id"].clone();
    let result = match body["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "serverInfo": { "name": "calc", "version": "0.0.1" }
        }),
        "tools/list" => json!({
            "tools": [{
                "name": "add",
                "description": "Add two integers.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
                    "required": ["a", "b"]
                }
            }]
        }),
        "tools/call" => {
            assert_eq!(body["params"]["name"], "add", "only `add` is offered");
            let args = &body["params"]["arguments"];
            let sum = args["a"].as_i64().unwrap_or(0) + args["b"].as_i64().unwrap_or(0);
            json!({
                "content": [{ "type": "text", "text": sum.to_string() }],
                "isError": false
            })
        }
        // Notifications (e.g. `notifications/initialized`) carry no id and get
        // no JSON-RPC response — a bare acknowledgement suffices.
        _ => return StatusCode::ACCEPTED.into_response(),
    };
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

/// Serve the mock `calc` MCP server on an ephemeral port; returns its base URL.
async fn mock_calc_mcp() -> String {
    let app = Router::new().route("/", post(rpc));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock mcp");
    let addr = listener.local_addr().expect("mock mcp addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock mcp");
    });
    format!("http://{addr}/")
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Create a vault holding an `mcp_oauth` credential for `url` whose access
/// token is the mock server's expected bearer; returns the vault id.
async fn vault_with_calc_credential(app: &Router, url: &str) -> String {
    let (s, vault) = call(
        app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "mcp" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let vault_id = vault["id"].as_str().unwrap().to_string();
    let (s, _) = call(
        app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "mcp_oauth",
            "mcp_server_url": url,
            "access_token": CALC_TOKEN
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    vault_id
}

async fn send_user_message(app: &Router, session: &str, text: &str) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        Some(json!({
            "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }]
        })),
    )
    .await
}

async fn list_events(app: &Router, session: &str) -> Vec<Value> {
    let (s, list) = call(app, "GET", &format!("/v1/sessions/{session}/events"), None).await;
    assert_eq!(s, StatusCode::OK);
    list["data"].as_array().expect("events data").clone()
}

/// The text of every `agent.message` event, in order.
fn agent_messages(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .map(|e| {
            e["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn session_inline_mcp_server_with_vault_credential_converses_multi_turn() {
    let url = mock_calc_mcp().await;
    let app = build_management_router();
    let vault_id = vault_with_calc_credential(&app, &url).await;

    // Session-inline binding: the session names the server, the vault supplies
    // the credential (matched by exact URL).
    let (s, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "assistant",
            "mcp_servers": [{ "name": "calc", "type": "url", "url": url }],
            "vault_ids": [vault_id],
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        session["agent"]["mcp_servers"],
        json!([{ "name": "calc", "type": "url", "url": url }])
    );
    let id = session["id"].as_str().unwrap().to_string();

    // Turn 1: the agent calls the MCP tool with the injected bearer and reports.
    let (s, _) = send_user_message(&app, &id, "add 2 3").await;
    assert_eq!(s, StatusCode::OK);
    let events = list_events(&app, &id).await;
    let tool_use = events
        .iter()
        .find(|e| e["type"] == "agent.tool_use")
        .expect("an agent.tool_use event");
    assert_eq!(tool_use["name"], "mcp__calc__add");
    let tool_result = events
        .iter()
        .find(|e| e["type"] == "agent.tool_result")
        .expect("an agent.tool_result event");
    assert_eq!(tool_result["content"][0]["text"], "5");
    assert!(
        agent_messages(&events).iter().any(|m| m.contains('5')),
        "final message reports 5: {events:?}"
    );

    // Turn 2 on the SAME session: the connection serves the next turn too.
    let (s, _) = send_user_message(&app, &id, "add 40 2").await;
    assert_eq!(s, StatusCode::OK);
    let events = list_events(&app, &id).await;
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "agent.tool_result" && e["content"][0]["text"] == "42"),
        "second turn's tool result is 42: {events:?}"
    );
    assert!(
        agent_messages(&events).iter().any(|m| m.contains("42")),
        "second turn's final message reports 42"
    );
}

#[tokio::test]
async fn management_plane_agent_mcp_config_takes_effect_without_inline_servers() {
    let url = mock_calc_mcp().await;
    let app = build_management_router();

    // Author the whole binding through the ADMIN routes: credential →
    // McpServerDef (Exact binding) → AgentMcpConfig for `calc-agent`.
    let (s, cred) = call(
        &app,
        "POST",
        "/v1/config/credentials",
        Some(json!({ "workspace_id": "ws", "kind": "vault", "secret": CALC_TOKEN })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let source_id = cred["id"].as_str().unwrap().to_string();
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/mcp-servers/calc-def",
        Some(json!({
            "id": "calc-def",
            "display_name": "calc",
            "url": url,
            "credential_binding": { "type": "exact", "credential_source_id": source_id },
            "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/agents/calc-agent/mcp",
        Some(json!({ "agent_id": "calc-agent", "mcp_server_ids": ["calc-def"], "version": 1 })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // A session for `calc-agent` with NO inline mcp_servers: the management
    // plane's config supplies the server + credential.
    let (s, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "calc-agent" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let id = session["id"].as_str().unwrap().to_string();

    let (s, _) = send_user_message(&app, &id, "add 2 3").await;
    assert_eq!(s, StatusCode::OK);
    let events = list_events(&app, &id).await;
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "agent.tool_use" && e["name"] == "mcp__calc__add"),
        "the authored MCP server's tool is called: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "agent.tool_result" && e["content"][0]["text"] == "5"),
        "the tool result is 5"
    );
    assert!(agent_messages(&events).iter().any(|m| m.contains('5')));
}

#[tokio::test]
async fn missing_vault_credential_fails_the_first_turn_loudly() {
    let url = mock_calc_mcp().await;
    let app = build_management_router();

    // The session names the server but binds NO vault: the prepared bearer is
    // None, so the mock answers 401 at the handshake.
    let (s, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "assistant",
            "mcp_servers": [{ "name": "calc", "type": "url", "url": url }],
        })),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "create binds lazily; the connect is per turn"
    );
    let id = session["id"].as_str().unwrap().to_string();

    // The first turn fails loudly with the error envelope — a configured MCP
    // server that cannot connect never silently vanishes.
    let (s, body) = send_user_message(&app, &id, "add 2 3").await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "api_error");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("mcp server `calc`"),
        "the failure names the server: {message}"
    );
}
