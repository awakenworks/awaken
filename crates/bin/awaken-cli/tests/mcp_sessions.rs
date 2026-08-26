//! End-to-end MCP over the management server (ADR-0043 Phase 3): a managed
//! session's MCP servers — bound either inline (session `mcp_servers` +
//! `vault_ids`) or through the management plane's agent↔MCP config — are
//! connected over real Streamable HTTP via `awaken-ext-mcp`, and the agent
//! calls the server's tool with the injected vault credential across a
//! multi-Run conversation.
//!
//! The in-process mock MCP server pins the fixture contract a Node e2e fixture
//! must mirror (JSON-RPC 2.0 over POST):
//! - `initialize` → any result (the client discards it), then the client fires
//!   the `notifications/initialized` notification (no `id`; answer 202);
//! - `tools/list` → `{ "tools": [{ "name": "add", "inputSchema": {...} }] }`;
//! - `tools/call` (`params.name = "add"`, `params.arguments = {a, b}`) →
//!   `{ "content": [{ "type": "text", "text": "<a+b>" }], "isError": false }`;
//! - EVERY request must carry `Authorization: Bearer <token>`, else 401 — the
//!   handshake fails and the Run fails loudly (never a silent skip).
//!
//! The server registers as name `calc`, so the runtime tool id is
//! `mcp__calc__add` (`awaken_ext_mcp::to_tool_id`), which is exactly what the
//! deterministic `McpToolModel` calls on `add <a> <b>`.
//!
//! The OAuth-mode mock ([`mock_oauth_calc_mcp`]) additionally pins the token
//! endpoint contract for the refresh-exchange fixture: `POST {base}/token`,
//! `application/x-www-form-urlencoded`, body
//! `grant_type=refresh_token&refresh_token=…[&scope=…][&resource=…]` →
//! `200 {"access_token": "…", "refresh_token"?: "…"}` (any non-2xx = grant
//! refused, the original 401 challenge surfaces). Client authentication per
//! the credential's `token_endpoint_auth` ([`MockClientAuth`]): `none` puts a
//! bare `client_id=…` in the form; `client_secret_basic` sends the RFC 6749
//! §2.3.1 `Authorization: Basic base64(urlencode(id):urlencode(secret))`
//! header and OMITS `client_id` from the form; `client_secret_post` puts
//! `client_id=…&client_secret=…` in the form.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use awaken_agent_contract::RedactedString;
use awaken_cli::build_all_in_one_router_with_model;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_materializer::VaultRefresher;
use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo};
use awaken_credential_vault::{
    CredentialKind, CredentialSource, CredentialStatus, InMemorySecretStore,
    OAUTH_CLIENT_SECRET_SLOT, OAUTH_REFRESH_TOKEN_SLOT, SecretRef, SecretStore,
};
use awaken_ext_mcp::{AuthChallenge, Credential, CredentialRefresher};
use awaken_runtime_contract::{CredentialRefreshAccess, TokenEndpointAuth};
use awaken_runtime_host::ExtMcpProbe;
use awaken_scenario_host::McpToolModel;
use awaken_session_contract::{McpProbe, McpProbeStatus};

// This test drives the REAL management router but needs a deterministic model that
// calls the MCP tool on `add <a> <b>` — that is a test concern, so it injects the
// (mock) `McpToolModel` through the test-only seam. Production uses the provider-free
// `NoModelConfiguredExecutor`; the mock never ships in the management process.
async fn build_all_in_one_router() -> Router {
    build_all_in_one_router_with_model(Arc::new(McpToolModel), "management").await
}

async fn exact_vault_refresher(
    access: CredentialRefreshAccess,
    secrets: Arc<dyn SecretStore>,
) -> (VaultRefresher, Arc<InMemoryCredentialRepo>) {
    let id = CredentialSourceId("cred:1".into());
    let mut auxiliary_material_refs = BTreeMap::from([(
        OAUTH_REFRESH_TOKEN_SLOT.to_string(),
        SecretRef(access.refresh_token_ref.clone()),
    )]);
    if let Some(reference) = &access.client_secret_ref {
        auxiliary_material_refs.insert(
            OAUTH_CLIENT_SECRET_SLOT.to_string(),
            SecretRef(reference.clone()),
        );
    }
    let repo = Arc::new(InMemoryCredentialRepo::new());
    repo.put(CredentialSource {
        id: id.clone(),
        replacement_of: None,
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        descriptor: None,
        provider_id: Some("mcp".into()),
        protocol_endpoint_id: None,
        env_key: None,
        material_ref: Some(SecretRef(access.access_token_ref.clone())),
        auxiliary_material_refs,
        oauth_command: None,
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: i64::try_from(access.credential_revision).unwrap(),
    })
    .await
    .unwrap();
    (VaultRefresher::new(id, access, repo.clone(), secrets), repo)
}
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

/// Session-local MCP policy for protocol/credential tests.
///
/// Causal graph:
/// MCP tool discovered -> policy evaluation -> allow executes / ask awaits / deny rejects
///                                      \-> transport and Vault assertions occur only on execute
///
/// Decision table for this suite:
/// | test responsibility        | policy       | expected boundary              |
/// | transport/Vault/OAuth      | always_allow | real MCP tool call/result       |
/// | human confirmation         | always_ask   | requires_action then confirmation|
/// | policy rejection           | deny/disabled| no MCP transport call           |
///
/// Confirmation and rejection are covered by the managed adapter policy tests;
/// this file owns the first row and opts in explicitly instead of weakening the
/// production `always_ask` default.
/// Exact official `agent_with_overrides` replacement for one Session MCP server.
/// The server and its toolset travel in the same typed Agent reference; there is
/// no parallel top-level Session extension.
fn always_allow_mcp_agent_with_server(id: &str, server_name: &str, url: &str) -> Value {
    json!({
        "id": id,
        "type": "agent_with_overrides",
        "mcp_servers": [{ "name": server_name, "type": "url", "url": url }],
        "tools": always_allow_mcp_tools(server_name)
    })
}

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

/// The client authentication the mock token endpoint requires, per scenario.
/// `None` on [`OauthMock::client_auth`] = a public client (no check).
enum MockClientAuth {
    /// Require EXACTLY this `Authorization` header value (the RFC 6749 §2.3.1
    /// Basic wire) AND reject any `client_id` in the form body.
    Basic { header: String },
    /// Require `client_secret=<this>` in the form body.
    Post { client_secret: String },
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
    /// The client authentication a grant must present; wrong/missing → 401
    /// `invalid_client` and no token is issued.
    client_auth: Option<MockClientAuth>,
    /// Raw form bodies the token endpoint received, one per grant attempt.
    grants: Vec<String>,
    /// The `Authorization` header of each grant attempt, aligned with `grants`.
    grant_authorizations: Vec<Option<String>>,
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

/// The mock token endpoint: records the raw form-encoded grant (and its
/// `Authorization` header), enforces the scenario's client authentication
/// ([`MockClientAuth`]), and either issues the configured response (whose
/// `access_token` becomes a valid bearer) or refuses — `401 invalid_client`
/// for bad client auth, `400 invalid_grant` when no response is configured.
async fn oauth_token(
    State(mock): State<Arc<Mutex<OauthMock>>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut m = mock.lock().unwrap();
    m.grants.push(body.clone());
    m.grant_authorizations.push(authorization.clone());
    if let Some(auth) = &m.client_auth {
        let authenticated = match auth {
            MockClientAuth::Basic { header } => {
                // The exact §2.3.1 header, and NO client_id in the form body.
                authorization.as_deref() == Some(header.as_str()) && !body.contains("client_id=")
            }
            MockClientAuth::Post { client_secret } => {
                body.contains(&format!("client_secret={client_secret}"))
            }
        };
        if !authenticated {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid_client" })),
            )
                .into_response();
        }
    }
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

/// Read the Managed projection until the sole Session lifecycle supervisor
/// commits the terminal boundary for this accepted command.
async fn wait_for_session_events(
    app: &Router,
    session: &str,
    accepted_receipt: &Value,
    expectation: &str,
) -> Vec<Value> {
    // Causes: C1 POST durably accepts a command and returns its last Event id;
    // C2 the lifecycle supervisor has not yet projected that Run's idle boundary;
    // C3 it has projected the anchor and that boundary; C4 the deadline expires.
    // Effects: E1 read/yield/retry; E2 return the complete committed projection;
    // E3 fail with the latest projection. Constraints: K1 this integration-test
    // observer performs GETs only and can neither execute nor reconcile a Run;
    // K2 anchoring excludes an idle Event from an earlier Run; K3 no sleep or
    // second lifecycle driver is permitted. Decision rules: W1=C1+C2=>E1;
    // W2=C1+C3=>E2; W3=C4=>E3.
    let receipt_anchor = accepted_receipt["data"]
        .as_array()
        .and_then(|events| events.last())
        .and_then(|event| event["id"].as_str())
        .unwrap_or_else(|| panic!("accepted Event receipt has no anchor: {accepted_receipt}"));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let uri = format!("/v1/sessions/{session}/events?limit=500");

    loop {
        let (status, list) = call(app, "GET", &uri, None).await;
        assert_eq!(status, StatusCode::OK, "GET {uri}: {list}");
        let events = list["data"]
            .as_array()
            .unwrap_or_else(|| panic!("GET {uri} has no Event data: {list}"));
        let causal_events = events
            .iter()
            .position(|event| event["id"] == receipt_anchor)
            .map(|position| &events[position..]);
        if causal_events.is_some_and(|events| {
            events
                .iter()
                .any(|event| event["type"] == "session.status_idle")
        }) {
            return events.clone();
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expectation} after receipt {receipt_anchor}; latest projection: {list}"
        );
        tokio::task::yield_now().await;
    }
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

/// These tests exercise MCP transport and credential behavior, so their
/// calculator is explicitly pre-authorized instead of depending on an ambient
/// permission default or entering the interactive approval path.
fn always_allow_mcp_tools(server_name: &str) -> Value {
    json!([{
        "type": "mcp_toolset",
        "mcp_server_name": server_name,
        "default_config": {
            "enabled": true,
            "permission_policy": { "type": "always_allow" }
        }
    }])
}

/// Cause/effect design: C1 a Session binds one inline MCP server to the exact
/// matching Vault credential; C2 two user messages run on that same Session;
/// C3 each accepted receipt may precede its lifecycle projection.
/// Effects: E1 the first Run calls `mcp__calc__add`, receives 5, and reports it;
/// E2 the second Run reuses the binding and receives/reports 42; E3 each read
/// waits for the same receipt's idle boundary. Constraint K1: the Vault binding
/// is selected by the exact server URL; K2 neither Run may rely on ambient
/// permission, a second credential path, or a test-owned driver. Decision rule
/// M1=C1+C2+C3=>E1+E2+E3; missing credentials are covered separately.
#[tokio::test(flavor = "multi_thread")]
async fn session_inline_mcp_server_with_vault_credential_converses_across_runs() {
    let url = mock_calc_mcp().await;
    let app = build_all_in_one_router().await;
    let vault_id = vault_with_calc_credential(&app, &url).await;

    // Session-inline binding: the session names the server, the vault supplies
    // the credential (matched by exact URL).
    let (s, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": always_allow_mcp_agent_with_server("assistant", "calc", &url),
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
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

    // Run 1: the agent calls the MCP tool with the injected bearer and reports.
    let (s, receipt) = send_user_message(&app, &id, "add 2 3").await;
    assert_eq!(s, StatusCode::OK);
    let events = wait_for_session_events(
        &app,
        &id,
        &receipt,
        "the first MCP Run to reach its aggregate idle boundary",
    )
    .await;
    let tool_use = events
        .iter()
        .find(|e| e["type"] == "agent.mcp_tool_use")
        .expect("an agent.mcp_tool_use event");
    assert_eq!(tool_use["name"], "mcp__calc__add");
    let tool_result = events
        .iter()
        .find(|e| e["type"] == "agent.mcp_tool_result")
        .expect("an agent.mcp_tool_result event");
    assert_eq!(tool_result["content"][0]["text"], "5");
    assert!(
        agent_messages(&events).iter().any(|m| m.contains('5')),
        "final message reports 5: {events:?}"
    );

    // Run 2 on the SAME Session: the connection serves the next Run too.
    let (s, receipt) = send_user_message(&app, &id, "add 40 2").await;
    assert_eq!(s, StatusCode::OK);
    let events = wait_for_session_events(
        &app,
        &id,
        &receipt,
        "the second MCP Run to reach its aggregate idle boundary",
    )
    .await;
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "agent.mcp_tool_result" && e["content"][0]["text"] == "42"),
        "second Run's tool result is 42: {events:?}"
    );
    assert!(
        agent_messages(&events).iter().any(|m| m.contains("42")),
        "second Run's final message reports 42"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn published_agent_mcp_binding_takes_effect_without_session_inline_servers() {
    // Causes: C1 the immutable Agent snapshot owns the MCP server binding; C2
    // the Session selects the exact Vault; C3 POST returns before asynchronous
    // lifecycle projection. Effects: E1 the Run invokes the published MCP tool;
    // E2 its result/final answer contain 5; E3 observation completes only after
    // the receipt-anchored idle boundary. Constraints: K1 no inline Session MCP
    // server may supply a parallel binding; K2 the observer is read-only.
    // Decision rule P1=C1+C2+C3=>E1+E2+E3.
    let url = mock_calc_mcp().await;
    let app = build_all_in_one_router().await;
    let vault_id = vault_with_calc_credential(&app, &url).await;

    // Author one typed Agent definition and publish its immutable executable
    // snapshot. MCP membership belongs to this aggregate; credential material
    // remains in the selected Vault and is injected only during provisioning.
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/agents/calc-agent",
        Some(json!({
            "name": "Calculator",
            "model": {
                "mode": "pinned",
                "provider_identity_ref": "default",
                "model_ref": "management",
                "backend_ref": "genai"
            },
            "system": "Use the calculator tool and report its result.",
            "mcp_servers": [{ "name": "calc", "url": url }],
            "tools": always_allow_mcp_tools("calc")
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&app, "POST", "/v1/config/agents/calc-agent/publish", None).await;
    assert_eq!(s, StatusCode::OK);

    // A session for `calc-agent` with NO inline mcp_servers: the management
    // plane's frozen Agent snapshot supplies the server and provisioning binds
    // its URL to the selected Vault credential.
    let (s, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "calc-agent",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
            "vault_ids": [vault_id]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let id = session["id"].as_str().unwrap().to_string();

    let (s, receipt) = send_user_message(&app, &id, "add 2 3").await;
    assert_eq!(s, StatusCode::OK);
    let events = wait_for_session_events(
        &app,
        &id,
        &receipt,
        "the published-Agent MCP Run to reach its aggregate idle boundary",
    )
    .await;
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "agent.mcp_tool_use" && e["name"] == "mcp__calc__add"),
        "the authored MCP server's tool is called: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "agent.mcp_tool_result" && e["content"][0]["text"] == "5"),
        "the tool result is 5: {events:?}"
    );
    assert!(
        agent_messages(&events).iter().any(|m| m.contains('5')),
        "final message reports 5: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_vault_credential_fails_initial_mcp_realization_loudly() {
    let url = mock_calc_mcp().await;
    let app = build_all_in_one_router().await;

    // The session names the server but binds NO vault: the prepared bearer is
    // None, so the mock answers 401 at the handshake.
    let (s, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": always_allow_mcp_agent_with_server("assistant", "calc", &url),
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        })),
    )
    .await;
    // Initial MCP generation 1 is realized before the Session becomes visible.
    // A configured server that cannot connect never produces a falsely healthy
    // Session or silently vanishes from its tool surface.
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(session["type"], "error");
    assert_eq!(session["error"]["type"], "api_error");
    let message = session["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("mcp server `calc`"),
        "the failure names the server: {message}"
    );
}

/// Create a vault holding an `mcp_oauth` credential for `url` with the given
/// (possibly expired) access token and a `cli-1` refresh configuration (client
/// auth per `token_endpoint_auth`) pointing at the mock's `{url}token`
/// endpoint; returns the vault id.
async fn vault_with_refreshable_credential(
    app: &Router,
    url: &str,
    access_token: &str,
    token_endpoint_auth: Value,
) -> String {
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
                "token_endpoint_auth": token_endpoint_auth,
                "scope": "tools"
            }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    vault_id
}

/// Attempt to create a session binding `url` as MCP server `calc` through
/// `vault_id`. Successful scenarios use [`create_mcp_session`]; failure scenarios
/// assert this exact initial-realization response.
async fn create_mcp_session_response(
    app: &Router,
    vault_id: &str,
    url: &str,
) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": always_allow_mcp_agent_with_server("assistant", "calc", url),
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
            "vault_ids": [vault_id],
        })),
    )
    .await
}

async fn create_mcp_session(app: &Router, vault_id: &str, url: &str) -> String {
    let (s, session) = create_mcp_session_response(app, vault_id, url).await;
    assert_eq!(s, StatusCode::OK, "{session}");
    session["id"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_mcp_oauth_token_is_refreshed_mid_connect_and_resealed() {
    // Test design — Causes: an expired bearer receives one OAuth challenge and
    // the refresh endpoint returns a usable replacement; each POST receipt may
    // precede lifecycle projection. Effects: the current Run retries with the
    // replacement, reseals it, a later Session reuses it without another grant,
    // and each observer reaches the receipt-anchored idle boundary. Constraints:
    // K1 the expired bearer is sent only on the challenged request; K2 credential
    // custody stays in the Vault; K3 the test never drives lifecycle work.
    // Decision rule R1: successful exchange + accepted receipts => one grant,
    // two successful terminal tool results, and no later expired bearer use.
    // The initial access token is EXPIRED (the mock accepts nothing until the
    // token endpoint issues `new-token`).
    let mock = Arc::new(Mutex::new(OauthMock {
        token_response: Some(json!({ "access_token": "new-token", "token_type": "Bearer" })), // awaken-allow: secret
        ..OauthMock::default()
    }));
    let url = mock_oauth_calc_mcp(mock.clone()).await;
    let app = build_all_in_one_router().await;
    let vault_id =
        vault_with_refreshable_credential(&app, &url, "expired-token", json!({ "type": "none" }))
            .await;
    let (status, session) = create_mcp_session_response(&app, &vault_id, &url).await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let id = session["id"].as_str().unwrap().to_string();

    // The Run still succeeds: the refresher exchanged the token mid-connect.
    let (s, receipt) = send_user_message(&app, &id, "add 2 3").await;
    assert_eq!(s, StatusCode::OK);
    let events = wait_for_session_events(
        &app,
        &id,
        &receipt,
        "the refreshed MCP Run to reach its aggregate idle boundary",
    )
    .await;
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "agent.mcp_tool_result" && e["content"][0]["text"] == "5"),
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
    let (s, receipt) = send_user_message(&app, &id2, "add 40 2").await;
    assert_eq!(s, StatusCode::OK);
    let events = wait_for_session_events(
        &app,
        &id2,
        &receipt,
        "the resealed-token MCP Run to reach its aggregate idle boundary",
    )
    .await;
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "agent.mcp_tool_result" && e["content"][0]["text"] == "42"),
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

#[tokio::test(flavor = "multi_thread")]
async fn refused_refresh_exchange_fails_initial_realization_with_the_challenge() {
    // `token_response: None` = the token endpoint answers 400 invalid_grant.
    let mock = Arc::new(Mutex::new(OauthMock::default()));
    let url = mock_oauth_calc_mcp(mock.clone()).await;
    let app = build_all_in_one_router().await;
    let vault_id =
        vault_with_refreshable_credential(&app, &url, "expired-token", json!({ "type": "none" }))
            .await;
    // Fail closed during initial generation realization: the refresher returned
    // None, so ext-mcp surfaces the original challenge and no Session is exposed.
    let (s, body) = create_mcp_session_response(&app, &vault_id, &url).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["type"], "api_error");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("mcp server `calc`"), "{message}");
    assert!(message.contains("auth challenge: HTTP 401"), "{message}");
    let m = mock.lock().unwrap();
    assert_eq!(m.grants.len(), 1, "the exchange was attempted exactly once");
}

// The confidential-client secret used across the basic-auth scenarios. It
// form-urlencodes to `s3cret+with+spaces%26`, so the tests pin that the pair is
// urlencoded BEFORE the base64 (RFC 6749 §2.3.1).
const BASIC_CLIENT_SECRET: &str = "s3cret with spaces&"; // awaken-allow: secret

/// The exact `Authorization` value the refresher must send for `cli-1` +
/// [`BASIC_CLIENT_SECRET`]: the encoded pair is hardcoded, so this pins the
/// urlencode-then-base64 wire rather than mirroring the implementation.
fn expected_basic_header() -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("cli-1:s3cret+with+spaces%26")
    )
}

/// Cause/effect design: C1 the stored access token is expired; C2 OAuth client
/// authentication is `client_secret_basic`; C3 the token endpoint accepts the
/// RFC 6749 encoded Basic header; C4 the POST receipt precedes or coincides with
/// lifecycle projection. Effects: E1 one refresh grant makes the MCP Run return
/// 5; E2 the grant body contains refresh fields but no client credentials; E3
/// the exact Basic header is sent once; E4 observation reaches the same Run's
/// idle boundary. Constraints: K1 client credentials exist only in the encoded
/// Basic header; K2 the refresh form must not duplicate either confidential
/// field; K3 the observer is read-only. Decision rule B1=C1+C2+C3+C4=>E1+E2+E3+E4.
#[tokio::test(flavor = "multi_thread")]
async fn expired_token_run_succeeds_with_client_secret_basic_refresh() {
    // The mock requires the exact §2.3.1 Basic header and REJECTS any
    // client_id in the form body.
    let mock = Arc::new(Mutex::new(OauthMock {
        token_response: Some(json!({ "access_token": "new-token", "token_type": "Bearer" })), // awaken-allow: secret
        client_auth: Some(MockClientAuth::Basic {
            header: expected_basic_header(),
        }),
        ..OauthMock::default()
    }));
    let url = mock_oauth_calc_mcp(mock.clone()).await;
    let app = build_all_in_one_router().await;
    let vault_id = vault_with_refreshable_credential(
        &app,
        &url,
        "expired-token",
        json!({ "type": "client_secret_basic", "client_secret": BASIC_CLIENT_SECRET }),
    )
    .await;
    let id = create_mcp_session(&app, &vault_id, &url).await;

    // The Run still succeeds: the grant authenticated with the Basic header.
    let (s, receipt) = send_user_message(&app, &id, "add 2 3").await;
    assert_eq!(s, StatusCode::OK);
    let events = wait_for_session_events(
        &app,
        &id,
        &receipt,
        "the Basic-auth MCP Run to reach its aggregate idle boundary",
    )
    .await;
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "agent.mcp_tool_result" && e["content"][0]["text"] == "5"),
        "the tool result is 5 despite the expired token: {events:?}"
    );

    let m = mock.lock().unwrap();
    assert_eq!(m.grants.len(), 1, "one challenge, one grant");
    let grant = &m.grants[0];
    assert!(grant.contains("grant_type=refresh_token"), "{grant}");
    assert!(grant.contains("refresh_token=rt-fixed"), "{grant}");
    // §2.3.1: the credentials ride in the header, and NEITHER client_id nor
    // client_secret appears in the form body.
    assert!(!grant.contains("client_id="), "{grant}");
    assert!(!grant.contains("client_secret="), "{grant}");
    assert_eq!(
        m.grant_authorizations[0].as_deref(),
        Some(expected_basic_header().as_str()),
        "the exact Basic wire"
    );
}

/// Cause/effect design: C1 the stored access token is expired; C2 OAuth client
/// authentication is `client_secret_post`; C3 the token endpoint accepts the
/// client id/secret in the form body; C4 the POST receipt precedes or coincides
/// with lifecycle projection. Effects: E1 one refresh grant makes the MCP Run
/// return 42; E2 both client fields are form encoded; E3 no Authorization header
/// is sent; E4 observation reaches the same Run's idle boundary. Constraints:
/// K1 `client_secret_post` has exactly one custody path—the encoded form; K2 it
/// must not manufacture a Basic header; K3 the observer is read-only. Decision
/// rule P1=C1+C2+C3+C4=>E1+E2+E3+E4.
#[tokio::test(flavor = "multi_thread")]
async fn expired_token_run_succeeds_with_client_secret_post_refresh() {
    let mock = Arc::new(Mutex::new(OauthMock {
        token_response: Some(json!({ "access_token": "new-token", "token_type": "Bearer" })), // awaken-allow: secret
        client_auth: Some(MockClientAuth::Post {
            client_secret: "cs-post-secret".to_string(), // awaken-allow: secret
        }),
        ..OauthMock::default()
    }));
    let url = mock_oauth_calc_mcp(mock.clone()).await;
    let app = build_all_in_one_router().await;
    let vault_id = vault_with_refreshable_credential(
        &app,
        &url,
        "expired-token",
        json!({ "type": "client_secret_post", "client_secret": "cs-post-secret" }), // awaken-allow: secret
    )
    .await;
    let id = create_mcp_session(&app, &vault_id, &url).await;

    let (s, receipt) = send_user_message(&app, &id, "add 40 2").await;
    assert_eq!(s, StatusCode::OK);
    let events = wait_for_session_events(
        &app,
        &id,
        &receipt,
        "the POST-auth MCP Run to reach its aggregate idle boundary",
    )
    .await;
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "agent.mcp_tool_result" && e["content"][0]["text"] == "42"),
        "the tool result is 42 despite the expired token: {events:?}"
    );

    let m = mock.lock().unwrap();
    assert_eq!(m.grants.len(), 1, "one challenge, one grant");
    let grant = &m.grants[0];
    // `client_secret_post`: client_id AND client_secret in the form body, no
    // Authorization header.
    assert!(grant.contains("client_id=cli-1"), "{grant}");
    assert!(grant.contains("client_secret=cs-post-secret"), "{grant}");
    assert_eq!(m.grant_authorizations[0], None, "no Basic header for post");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_client_secret_refuses_initial_realization_and_surfaces_the_challenge() {
    // Test design — Causes: an expired bearer is paired with a Basic client
    // secret that does not satisfy the token endpoint. Effects: realization
    // fails with the original MCP authentication challenge after one exchange.
    // Constraints/invariants: no Session or replacement token is admitted on a
    // refused confidential-client grant. Decision rule F1: wrong secret => one
    // failed grant and a fail-closed API error containing the server challenge.
    // The mock demands the right Basic creds; the credential was entered with
    // a DIFFERENT secret, so the exchange is refused and the original 401
    // challenge fails the Run loudly.
    let mock = Arc::new(Mutex::new(OauthMock {
        token_response: Some(json!({ "access_token": "new-token", "token_type": "Bearer" })), // awaken-allow: secret
        client_auth: Some(MockClientAuth::Basic {
            header: expected_basic_header(),
        }),
        ..OauthMock::default()
    }));
    let url = mock_oauth_calc_mcp(mock.clone()).await;
    let app = build_all_in_one_router().await;
    let vault_id = vault_with_refreshable_credential(
        &app,
        &url,
        "expired-token",
        json!({ "type": "client_secret_basic", "client_secret": "not-the-secret" }), // awaken-allow: secret
    )
    .await;
    let (s, body) = create_mcp_session_response(&app, &vault_id, &url).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["type"], "api_error");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("mcp server `calc`"), "{message}");
    assert!(message.contains("auth challenge: HTTP 401"), "{message}");
    let m = mock.lock().unwrap();
    assert_eq!(m.grants.len(), 1, "the exchange was attempted exactly once");
}

#[tokio::test(flavor = "multi_thread")]
async fn vault_refresher_fails_closed_when_the_client_secret_is_missing() {
    // A confidential binding whose sealed client secret is GONE from the store:
    // the exchange is never attempted (fail closed), nothing is resealed.
    let mock = Arc::new(Mutex::new(OauthMock {
        token_response: Some(json!({ "access_token": "new-token" })), // awaken-allow: secret
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

    let (refresher, _) = exact_vault_refresher(
        CredentialRefreshAccess::new(
            1,
            format!("{url}token"),
            "cli-1".to_string(),
            TokenEndpointAuth::ClientSecretBasic,
            Some("sec:client:missing".to_string()),
            refresh_ref.0.clone(),
            access_ref.0.clone(),
            None,
            None,
        ),
        secrets.clone(),
    )
    .await;
    let fresh = refresher
        .refresh(&AuthChallenge {
            status: 401,
            www_authenticate: None,
        })
        .await;
    assert_eq!(fresh, None, "a missing client secret refuses the exchange");
    assert!(
        mock.lock().unwrap().grants.is_empty(),
        "the token endpoint was never contacted"
    );
    assert_eq!(
        secrets.get(&access_ref).await.unwrap().expose_secret(),
        "expired-token",
        "nothing was resealed"
    );
}

#[tokio::test(flavor = "multi_thread")]
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

    let (refresher, repo) = exact_vault_refresher(
        CredentialRefreshAccess::new(
            1,
            format!("{url}token"),
            "cli-1".to_string(),
            TokenEndpointAuth::None,
            None,
            refresh_ref.0.clone(),
            access_ref.0.clone(),
            None,
            Some("https://mcp.example.com".to_string()),
        ),
        secrets.clone(),
    )
    .await;
    let fresh = refresher
        .refresh(&AuthChallenge {
            status: 401,
            www_authenticate: None,
        })
        .await;
    assert_eq!(fresh, Some(Credential::Bearer("new-token".to_string())));

    // BOTH secrets moved to one higher exact revision, so later
    // sessions/validations get the fresh pair and old refs are reclaimed; the
    // grant carried the optional `resource` parameter form-encoded.
    let current = repo
        .get(&CredentialSourceId("cred:1".into()))
        .await
        .unwrap();
    assert_eq!(
        secrets
            .get(current.material_ref.as_ref().unwrap())
            .await
            .unwrap()
            .expose_secret(),
        "new-token"
    );
    assert_eq!(
        secrets
            .get(
                current
                    .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)
                    .unwrap(),
            )
            .await
            .unwrap()
            .expose_secret(),
        "rt-rotated"
    );
    assert!(secrets.get(&access_ref).await.is_err());
    assert!(secrets.get(&refresh_ref).await.is_err());
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

#[tokio::test(flavor = "multi_thread")]
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

    let (refresher, _) = exact_vault_refresher(
        CredentialRefreshAccess::new(
            1,
            format!("{url}token"),
            "cli-1".to_string(),
            TokenEndpointAuth::None,
            None,
            refresh_ref.0.clone(),
            access_ref.0.clone(),
            None,
            None,
        ),
        secrets.clone(),
    )
    .await;
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

#[tokio::test(flavor = "multi_thread")]
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

#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_validate_live_probes_over_the_management_router() {
    let url = mock_calc_mcp().await;
    let app = build_all_in_one_router().await;
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
