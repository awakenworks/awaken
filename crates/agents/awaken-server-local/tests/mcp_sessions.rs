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
//!
//! The OAuth-mode mock ([`mock_oauth_calc_mcp`]) additionally pins the token
//! endpoint contract for the refresh-exchange fixture: `POST {base}/token`,
//! `application/x-www-form-urlencoded`, body
//! `grant_type=refresh_token&refresh_token=…&client_id=…[&scope=…][&resource=…]`
//! → `200 {"access_token": "…", "refresh_token"?: "…"}` (any non-2xx = grant
//! refused, the original 401 challenge surfaces).

use std::sync::{Arc, Mutex};

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::{InMemorySecretStore, SecretRef, SecretStore};
use awaken_ext_mcp::{AuthChallenge, Credential, CredentialRefresher};
use awaken_protocol_managed::{McpProbe, McpProbeStatus};
use awaken_server_local::{
    ExtMcpProbe, PreparedMcpRefresh, VaultRefresher, build_management_router,
};
use axum::Router;
use axum::body::Body;
use axum::extract::{Json, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const CALC_TOKEN: &str = "calc-bearer-token"; // awaken-allow: secret

/// Answer one authorized JSON-RPC request of the `calc` contract.
fn calc_rpc_result(body: &Value) -> Response {
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
    calc_rpc_result(&body)
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

/// The OAuth-mode mock's shared state: which bearers the MCP endpoint accepts
/// (a token issued by the token endpoint becomes valid), what the token
/// endpoint answers, and everything both endpoints observed.
#[derive(Default)]
struct OauthMock {
    /// Bearer tokens the JSON-RPC endpoint currently accepts.
    valid_tokens: Vec<String>,
    /// The token endpoint's issue body; `None` = 400 `invalid_grant`.
    token_response: Option<Value>,
    /// Raw form bodies the token endpoint received, one per grant attempt.
    grants: Vec<String>,
    /// `(method, authorization header)` of every JSON-RPC request, in order.
    requests: Vec<(String, String)>,
}

/// The JSON-RPC endpoint of the OAuth-mode mock: any bearer outside
/// `valid_tokens` gets `401` + `WWW-Authenticate` (the challenge that triggers
/// the host refresher).
async fn oauth_rpc(
    State(mock): State<Arc<Mutex<OauthMock>>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let method = body["method"].as_str().unwrap_or_default().to_string();
    let authorized = {
        let mut m = mock.lock().unwrap();
        m.requests.push((method, bearer.clone()));
        m.valid_tokens
            .iter()
            .any(|t| bearer == format!("Bearer {t}"))
    };
    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Bearer realm=\"calc\"")],
        )
            .into_response();
    }
    calc_rpc_result(&body)
}

/// The mock token endpoint: records the raw form-encoded grant and either
/// issues the configured response (whose `access_token` becomes a valid bearer)
/// or refuses with `400 invalid_grant`.
async fn oauth_token(State(mock): State<Arc<Mutex<OauthMock>>>, body: String) -> Response {
    let mut m = mock.lock().unwrap();
    m.grants.push(body);
    match m.token_response.clone() {
        Some(issued) => {
            if let Some(token) = issued["access_token"].as_str() {
                m.valid_tokens.push(token.to_string());
            }
            Json(issued).into_response()
        }
        None => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_grant" })),
        )
            .into_response(),
    }
}

/// Serve the OAuth-mode mock (`/` JSON-RPC + `/token` token endpoint) on an
/// ephemeral port; returns its base URL.
async fn mock_oauth_calc_mcp(mock: Arc<Mutex<OauthMock>>) -> String {
    let app = Router::new()
        .route("/", post(oauth_rpc))
        .route("/token", post(oauth_token))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind oauth mock mcp");
    let addr = listener.local_addr().expect("oauth mock mcp addr");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve oauth mock mcp");
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

/// Create a vault holding an `mcp_oauth` credential for `url` with the given
/// (possibly expired) access token and a public-client refresh configuration
/// pointing at the mock's `{url}token` endpoint; returns the vault id.
async fn vault_with_refreshable_credential(app: &Router, url: &str, access_token: &str) -> String {
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
            "access_token": access_token,
            "refresh": {
                "client_id": "cli-1",
                "refresh_token": "rt-fixed", // awaken-allow: secret
                "token_endpoint": format!("{url}token"),
                "token_endpoint_auth": { "type": "none" },
                "scope": "tools"
            }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    vault_id
}

/// Create a session binding `url` as MCP server `calc` through `vault_id`.
async fn create_mcp_session(app: &Router, vault_id: &str, url: &str) -> String {
    let (s, session) = call(
        app,
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
    session["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn expired_mcp_oauth_token_is_refreshed_mid_connect_and_resealed() {
    // The initial access token is EXPIRED (the mock accepts nothing until the
    // token endpoint issues `new-token`).
    let mock = Arc::new(Mutex::new(OauthMock {
        token_response: Some(json!({ "access_token": "new-token", "token_type": "Bearer" })), // awaken-allow: secret
        ..OauthMock::default()
    }));
    let url = mock_oauth_calc_mcp(mock.clone()).await;
    let app = build_management_router();
    let vault_id = vault_with_refreshable_credential(&app, &url, "expired-token").await;
    let id = create_mcp_session(&app, &vault_id, &url).await;

    // The turn still succeeds: the refresher exchanged the token mid-connect.
    let (s, _) = send_user_message(&app, &id, "add 2 3").await;
    assert_eq!(s, StatusCode::OK);
    let events = list_events(&app, &id).await;
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "agent.tool_result" && e["content"][0]["text"] == "5"),
        "the tool result is 5 despite the expired token: {events:?}"
    );

    {
        let m = mock.lock().unwrap();
        // Exactly one refresh grant, carrying the documented wire.
        assert_eq!(m.grants.len(), 1, "one challenge, one grant");
        let grant = &m.grants[0];
        assert!(grant.contains("grant_type=refresh_token"), "{grant}");
        assert!(grant.contains("refresh_token=rt-fixed"), "{grant}");
        assert!(grant.contains("client_id=cli-1"), "{grant}");
        assert!(grant.contains("scope=tools"), "{grant}");
        // The first request carried the expired bearer; after the exchange the
        // retried handshake and the actual tool call carried `new-token`.
        assert_eq!(m.requests[0].0, "initialize");
        assert_eq!(m.requests[0].1, "Bearer expired-token");
        assert_eq!(
            m.requests
                .iter()
                .filter(|(_, bearer)| bearer.as_str() == "Bearer expired-token")
                .count(),
            1,
            "the expired bearer is never sent again after the refresh"
        );
        let tool_call = m
            .requests
            .iter()
            .find(|(method, _)| method == "tools/call")
            .expect("a tools/call reached the mock");
        assert_eq!(tool_call.1, "Bearer new-token");
    }

    // The fresh access token was RESEALED under the credential row: a second
    // session on the same credential materializes `new-token` and connects
    // without another token-endpoint hit.
    let id2 = create_mcp_session(&app, &vault_id, &url).await;
    let (s, _) = send_user_message(&app, &id2, "add 40 2").await;
    assert_eq!(s, StatusCode::OK);
    let events = list_events(&app, &id2).await;
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "agent.tool_result" && e["content"][0]["text"] == "42"),
        "the second session's tool result is 42: {events:?}"
    );
    let m = mock.lock().unwrap();
    assert_eq!(
        m.grants.len(),
        1,
        "the resealed token connects the second session with no new grant"
    );
    assert!(
        !m.requests[m.requests.len().saturating_sub(4)..]
            .iter()
            .any(|(_, bearer)| bearer.as_str() == "Bearer expired-token"),
        "the second session never presented the expired token"
    );
}

#[tokio::test]
async fn refused_refresh_exchange_fails_the_turn_with_the_challenge() {
    // `token_response: None` = the token endpoint answers 400 invalid_grant.
    let mock = Arc::new(Mutex::new(OauthMock::default()));
    let url = mock_oauth_calc_mcp(mock.clone()).await;
    let app = build_management_router();
    let vault_id = vault_with_refreshable_credential(&app, &url, "expired-token").await;
    let id = create_mcp_session(&app, &vault_id, &url).await;

    // Fail closed: the refresher returned None, so ext-mcp surfaces the original
    // auth challenge and the turn fails loudly, naming the server.
    let (s, body) = send_user_message(&app, &id, "add 2 3").await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["type"], "api_error");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("mcp server `calc`"), "{message}");
    assert!(message.contains("auth challenge: HTTP 401"), "{message}");
    let m = mock.lock().unwrap();
    assert_eq!(m.grants.len(), 1, "the exchange was attempted exactly once");
}

#[tokio::test]
async fn vault_refresher_reseals_the_access_token_and_a_rotated_refresh_token() {
    let mock = Arc::new(Mutex::new(OauthMock {
        // The endpoint also ROTATES the refresh token.
        token_response: Some(
            json!({ "access_token": "new-token", "refresh_token": "rt-rotated" }), // awaken-allow: secret
        ),
        ..OauthMock::default()
    }));
    let url = mock_oauth_calc_mcp(mock.clone()).await;
    let secrets: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());
    let access_ref = SecretRef("sec:cred:1".to_string());
    let refresh_ref = SecretRef("sec:refresh:cred:1".to_string());
    secrets
        .put(
            &access_ref,
            RedactedString::new("expired-token".to_string()),
        )
        .await
        .unwrap();
    secrets
        .put(&refresh_ref, RedactedString::new("rt-fixed".to_string()))
        .await
        .unwrap();

    let refresher = VaultRefresher::new(PreparedMcpRefresh {
        token_endpoint: format!("{url}token"),
        client_id: "cli-1".to_string(),
        scope: None,
        resource: Some("https://mcp.example.com".to_string()),
        refresh_token_ref: refresh_ref.clone(),
        access_token_ref: access_ref.clone(),
        secrets: secrets.clone(),
    });
    let fresh = refresher
        .refresh(&AuthChallenge {
            status: 401,
            www_authenticate: None,
        })
        .await;
    assert_eq!(fresh, Some(Credential::Bearer("new-token".to_string())));

    // BOTH secrets were resealed, so later sessions/validations get the fresh
    // pair; the grant carried the optional `resource` parameter form-encoded.
    assert_eq!(
        secrets.get(&access_ref).await.unwrap().expose_secret(),
        "new-token"
    );
    assert_eq!(
        secrets.get(&refresh_ref).await.unwrap().expose_secret(),
        "rt-rotated"
    );
    let m = mock.lock().unwrap();
    assert_eq!(m.grants.len(), 1);
    assert!(
        m.grants[0].contains("resource=https%3A%2F%2Fmcp.example.com"),
        "{}",
        m.grants[0]
    );
    assert!(
        m.grants[0].contains("refresh_token=rt-fixed"),
        "{}",
        m.grants[0]
    );
}

#[tokio::test]
async fn vault_refresher_fails_closed_and_reseals_nothing_on_a_rejected_grant() {
    let mock = Arc::new(Mutex::new(OauthMock::default())); // 400 invalid_grant
    let url = mock_oauth_calc_mcp(mock.clone()).await;
    let secrets: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());
    let access_ref = SecretRef("sec:cred:1".to_string());
    let refresh_ref = SecretRef("sec:refresh:cred:1".to_string());
    secrets
        .put(
            &access_ref,
            RedactedString::new("expired-token".to_string()),
        )
        .await
        .unwrap();
    secrets
        .put(&refresh_ref, RedactedString::new("rt-fixed".to_string()))
        .await
        .unwrap();

    let refresher = VaultRefresher::new(PreparedMcpRefresh {
        token_endpoint: format!("{url}token"),
        client_id: "cli-1".to_string(),
        scope: None,
        resource: None,
        refresh_token_ref: refresh_ref.clone(),
        access_token_ref: access_ref.clone(),
        secrets: secrets.clone(),
    });
    let fresh = refresher
        .refresh(&AuthChallenge {
            status: 401,
            www_authenticate: None,
        })
        .await;
    assert_eq!(fresh, None, "a refused grant yields no credential");
    // Nothing was resealed — the stored pair is untouched.
    assert_eq!(
        secrets.get(&access_ref).await.unwrap().expose_secret(),
        "expired-token"
    );
    assert_eq!(
        secrets.get(&refresh_ref).await.unwrap().expose_secret(),
        "rt-fixed"
    );
}

#[tokio::test]
async fn ext_mcp_probe_classifies_valid_invalid_and_unknown() {
    let url = mock_calc_mcp().await;
    let probe = ExtMcpProbe;
    assert_eq!(
        probe
            .probe(&url, &RedactedString::new(CALC_TOKEN.to_string()))
            .await,
        McpProbeStatus::Valid,
        "handshake success with the accepted bearer"
    );
    assert_eq!(
        probe
            .probe(&url, &RedactedString::new("wrong-token".to_string()))
            .await,
        McpProbeStatus::Invalid { http_status: 401 },
        "a refused bearer is invalid, carrying the challenge status"
    );
    assert_eq!(
        probe
            .probe("http://127.0.0.1:1/", &RedactedString::new("x".to_string()))
            .await,
        McpProbeStatus::Unknown,
        "an unreachable server yields no verdict"
    );
}

#[tokio::test]
async fn mcp_oauth_validate_live_probes_over_the_management_router() {
    let url = mock_calc_mcp().await;
    let app = build_management_router();
    let (s, vault) = call(
        &app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "mcp" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let vault_id = vault["id"].as_str().unwrap().to_string();

    // (access token, server url, expected status, expected probe detail)
    let arms: [(&str, String, &str, Value); 3] = [
        (
            CALC_TOKEN,
            url.clone(),
            "valid",
            json!({ "handshake": "ok" }),
        ),
        (
            "wrong-token",
            url.clone(),
            "invalid",
            json!({ "http_status": 401 }),
        ),
        (
            "whatever",
            "http://127.0.0.1:1/".to_string(),
            "unknown",
            Value::Null,
        ),
    ];
    for (token, server_url, expected_status, expected_probe) in arms {
        let (s, cred) = call(
            &app,
            "POST",
            &format!("/v1/vaults/{vault_id}/credentials"),
            Some(json!({
                "type": "mcp_oauth",
                "mcp_server_url": server_url,
                "access_token": token
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let cred_id = cred["id"].as_str().unwrap().to_string();
        let (s, validation) = call(
            &app,
            "POST",
            &format!("/v1/vaults/{vault_id}/credentials/{cred_id}/mcp_oauth_validate"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(validation["status"], expected_status, "{validation:?}");
        assert_eq!(validation["mcp_probe"], expected_probe, "{validation:?}");
        // The probe detail never carries the token.
        assert!(
            !serde_json::to_string(&validation).unwrap().contains(token),
            "{validation:?}"
        );
    }
}
