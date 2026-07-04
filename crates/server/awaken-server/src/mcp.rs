//! MCP composition for the shared host (ADR-0043 Phase 3).
//!
//! Owns the wire-client half of per-thread MCP wiring: given the servers a
//! session staged (already credential-materialized), connect each through
//! `awaken-ext-mcp` and hand back the executable tools + model-visible
//! descriptors the host registers on that thread's runtime. Split out of
//! `host.rs` so the host keeps session orchestration and this module owns the
//! outbound wire composition.
//!
//! It also owns the two host-side OAuth pieces of the managed vault design:
//! [`VaultRefresher`] (the `CredentialRefresher` the HTTP transport consults on
//! a 401/403 — it runs the RFC 6749 `refresh_token` grant and reseals the
//! rotated secrets back into the vault) and [`ExtMcpProbe`] (the
//! `awaken_protocol_managed::McpProbe` port the `mcp_oauth_validate` route
//! drives — a connect + `initialize` handshake as the live credential check).

use std::sync::{Arc, Mutex};

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::{SecretRef, SecretStore};
use awaken_ext_mcp::{AuthChallenge, Credential, CredentialRefresher, HttpTransportBuilder};
use awaken_protocol_managed::{McpProbe, McpProbeStatus, TokenEndpointAuthBinding};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::RawTool;
use base64::Engine as _;

use crate::host::HostError;

/// An MCP server prepared for one thread (ADR-0043 Phase 3): the connection
/// target plus an already-materialized bearer token (`None` = connect
/// unauthenticated and let the server decide). Registered before the thread's
/// first turn via [`SharedHost::register_thread_mcp`](crate::SharedHost::register_thread_mcp)
/// and consumed by the host's session build, which connects through
/// `awaken-ext-mcp` and registers the discovered tools.
/// Only the resolved `RedactedString` crosses in (D6/D9) — never a vault ref.
#[derive(Clone)]
pub struct PreparedMcpServer {
    pub name: String,
    pub url: String,
    pub bearer: Option<awaken_agent_contract::RedactedString>,
    /// The vault credential's refresh configuration, when it has one: the
    /// connect then registers a [`VaultRefresher`] so an expired access token
    /// is exchanged mid-request instead of failing the turn.
    pub refresh: Option<PreparedMcpRefresh>,
}

/// The refresh half of a prepared MCP server (an `mcp_oauth` vault credential
/// entered with a refresh object — public or confidential client): everything
/// [`VaultRefresher`] needs to run the `refresh_token` grant and reseal the
/// results. Carries [`SecretRef`]s plus the [`SecretStore`] handle — never
/// secret material.
#[derive(Clone)]
pub struct PreparedMcpRefresh {
    pub token_endpoint: String,
    pub client_id: String,
    /// How the grant authenticates at the token endpoint: `none` (public
    /// client), or a confidential scheme carrying the sealed client secret's
    /// ref ([`VaultRefresher`] reads it per exchange).
    pub token_endpoint_auth: TokenEndpointAuthBinding,
    pub scope: Option<String>,
    pub resource: Option<String>,
    /// Where the sealed refresh token lives (read per exchange; rewritten when
    /// the token endpoint rotates it).
    pub refresh_token_ref: SecretRef,
    /// The credential row's `material_ref` — the fresh access token is resealed
    /// under it, so later sessions (and the validate probe) get the new secret.
    pub access_token_ref: SecretRef,
    pub secrets: Arc<dyn SecretStore>,
}

/// The host-side [`CredentialRefresher`] of the managed vault design (ADR-0043):
/// consulted by the ext-mcp HTTP transport once per auth challenge. It performs
/// an RFC 6749 `refresh_token` grant against the credential's stored token
/// endpoint — `POST` form-encoded `grant_type=refresh_token&refresh_token=…`
/// (+`scope`/`resource` when configured), with client authentication per the
/// stored [`TokenEndpointAuthBinding`]:
/// - `none` (public client): `client_id=…` in the form body;
/// - `client_secret_basic`: `Authorization: Basic
///   base64(urlencode(client_id):urlencode(client_secret))` (RFC 6749 §2.3.1),
///   and the `client_id` stays OUT of the form body;
/// - `client_secret_post`: `client_id=…&client_secret=…` in the form body.
///
/// The confidential-client secret is read from the vault at refresh time; a
/// missing/unreadable sealed secret refuses the exchange (`None`). On success it
/// reseals the new access token under the credential row's `material_ref` and
/// any rotated `refresh_token` under the refresh ref, then hands the transport
/// the fresh bearer to retry with. ANY failure (network, non-2xx, malformed
/// JSON, a reseal error) returns `None`, so the transport surfaces the original
/// challenge — fail closed, never a panic.
pub struct VaultRefresher {
    refresh: PreparedMcpRefresh,
    http: reqwest::Client,
}

/// The RFC 6749 §2.3.1 `client_secret_basic` header value:
/// `Basic base64(urlencode(client_id):urlencode(client_secret))` — both halves
/// form-urlencoded BEFORE the base64, as the RFC requires.
fn basic_client_auth(client_id: &str, client_secret: &str) -> String {
    let enc = |s: &str| form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let pair = format!("{}:{}", enc(client_id), enc(client_secret));
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}

impl VaultRefresher {
    #[must_use]
    pub fn new(refresh: PreparedMcpRefresh) -> Self {
        Self {
            refresh,
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl CredentialRefresher for VaultRefresher {
    async fn refresh(&self, _challenge: &AuthChallenge) -> Option<Credential> {
        let r = &self.refresh;
        let refresh_token = r.secrets.get(&r.refresh_token_ref).await.ok()?;
        // A confidential scheme's sealed client secret is read HERE, per
        // exchange; missing/unreadable → None (fail closed: the transport
        // surfaces the original challenge).
        let client_secret = match &r.token_endpoint_auth {
            TokenEndpointAuthBinding::ClientSecretBasic { secret_ref }
            | TokenEndpointAuthBinding::ClientSecretPost { secret_ref } => {
                Some(r.secrets.get(secret_ref).await.ok()?)
            }
            TokenEndpointAuthBinding::None => None,
        };
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.expose_secret()),
        ];
        let mut request = self.http.post(&r.token_endpoint);
        match &r.token_endpoint_auth {
            // Public client: the bare client_id rides in the form body.
            TokenEndpointAuthBinding::None => form.push(("client_id", r.client_id.as_str())),
            // RFC 6749 §2.3.1: HTTP Basic with the form-urlencoded credential
            // pair; the client_id is OMITTED from the form body.
            TokenEndpointAuthBinding::ClientSecretBasic { .. } => {
                request = request.header(
                    reqwest::header::AUTHORIZATION,
                    basic_client_auth(&r.client_id, client_secret.as_ref()?.expose_secret()),
                );
            }
            // RFC 6749 §2.3.1 form alternative: client_id + client_secret in
            // the body.
            TokenEndpointAuthBinding::ClientSecretPost { .. } => {
                form.push(("client_id", r.client_id.as_str()));
                form.push(("client_secret", client_secret.as_ref()?.expose_secret()));
            }
        }
        if let Some(scope) = &r.scope {
            form.push(("scope", scope.as_str()));
        }
        if let Some(resource) = &r.resource {
            form.push(("resource", resource.as_str()));
        }
        let response = request.form(&form).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response.json().await.ok()?;
        let access_token = body.get("access_token")?.as_str()?.to_string();
        // Reseal BEFORE handing the token out: later sessions and the validate
        // probe materialize the row and must see the fresh secret.
        r.secrets
            .put(
                &r.access_token_ref,
                RedactedString::new(access_token.clone()),
            )
            .await
            .ok()?;
        if let Some(rotated) = body.get("refresh_token").and_then(|v| v.as_str()) {
            r.secrets
                .put(
                    &r.refresh_token_ref,
                    RedactedString::new(rotated.to_string()),
                )
                .await
                .ok()?;
        }
        Some(Credential::Bearer(access_token))
    }
}

/// Records the auth challenge the probe's connect attempt hit (and declines to
/// refresh), so [`ExtMcpProbe`] can tell a refused bearer apart from an
/// unreachable server.
struct ChallengeCapture {
    status: Mutex<Option<u16>>,
}

#[async_trait::async_trait]
impl CredentialRefresher for ChallengeCapture {
    async fn refresh(&self, challenge: &AuthChallenge) -> Option<Credential> {
        *self.status.lock().unwrap() = Some(challenge.status);
        None
    }
}

/// The live MCP credential probe (ADR-0043), implementing the
/// [`awaken_protocol_managed::McpProbe`] port over `awaken-ext-mcp`: connect +
/// MCP `initialize` handshake with the materialized bearer. Handshake success →
/// `Valid`; an auth challenge (401/403) → `Invalid` with the HTTP status;
/// anything else (unreachable, protocol error) → `Unknown`. This is the only
/// place the MCP client is named for validation — the adapter crate stays
/// wire-client-free.
pub struct ExtMcpProbe;

#[async_trait::async_trait]
impl McpProbe for ExtMcpProbe {
    async fn probe(&self, mcp_server_url: &str, bearer: &RedactedString) -> McpProbeStatus {
        let capture = Arc::new(ChallengeCapture {
            status: Mutex::new(None),
        });
        let connected = HttpTransportBuilder::new(mcp_server_url.to_string())
            .credential(Credential::Bearer(bearer.expose_secret().to_string()))
            .refresher(Arc::clone(&capture) as Arc<dyn CredentialRefresher>)
            .connect()
            .await;
        let challenged = *capture.status.lock().unwrap();
        match (connected, challenged) {
            (Ok(_), _) => McpProbeStatus::Valid,
            (Err(_), Some(http_status)) => McpProbeStatus::Invalid { http_status },
            (Err(_), None) => McpProbeStatus::Unknown,
        }
    }
}

/// The discovered surface of one thread's staged MCP servers: the executable
/// tools to register on the runtime, their model-visible descriptors for the
/// advertised config, and the namespaced tool ids the gate pre-authorizes.
pub struct McpWiring {
    pub tools: Vec<Arc<dyn RawTool>>,
    pub descriptors: Vec<ToolDescriptor>,
    pub tool_ids: Vec<String>,
}

/// Connect every staged server and collect the discovered tools. Fail closed:
/// a configured server that cannot connect (network, 401, bad wire) fails the
/// build — never a silent skip — naming the server so the error is actionable.
/// A server with refresh configuration gets a [`VaultRefresher`], so an expired
/// access token is exchanged mid-connect (and mid-turn) instead of failing.
pub async fn connect_staged(staged: &[PreparedMcpServer]) -> Result<McpWiring, HostError> {
    let mut wiring = McpWiring {
        tools: Vec::new(),
        descriptors: Vec::new(),
        tool_ids: Vec::new(),
    };
    for server in staged {
        let credential = match &server.bearer {
            Some(token) => awaken_ext_mcp::Credential::Bearer(token.expose_secret().to_string()),
            None => awaken_ext_mcp::Credential::None,
        };
        let mut builder = HttpTransportBuilder::new(server.url.clone()).credential(credential);
        if let Some(refresh) = &server.refresh {
            builder =
                builder
                    .refresher(Arc::new(VaultRefresher::new(refresh.clone()))
                        as Arc<dyn CredentialRefresher>);
        }
        let transport = builder.connect().await.map_err(|e| {
            HostError::internal(format!(
                "mcp server `{}` at {}: {e}",
                server.name, server.url
            ))
        })?;
        let connection = awaken_ext_mcp::connect_tools(&server.name, Arc::new(transport))
            .await
            .map_err(|e| HostError::internal(format!("mcp server `{}`: {e}", server.name)))?;
        wiring
            .tool_ids
            .extend(connection.descriptors.iter().map(|d| d.id.clone()));
        wiring.descriptors.extend(connection.descriptors);
        wiring.tools.extend(connection.tools);
    }
    Ok(wiring)
}
