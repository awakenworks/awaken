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

/// Project a staged MCP server to the neutral ACP config the ACP executor reads back
/// from `plugin_config.acp.mcp_servers`, choosing the credential form by **trust**:
/// - a **trusted** (non-sandboxed local) run may carry the raw bearer **inline** (β);
/// - a **sandboxed** (untrusted) run gets a **secretless reference** (α) — the raw bearer
///   never enters the sandbox; a broker/gateway resolves the reference out-of-band. A
///   server with no bearer is `None`.
///
/// This is the α/β decision point: the host owns the raw secret and decides, per the
/// run's isolation, whether the CLI may see it. (For an ACP run the servers are the
/// CLI's own MCP client's; native runs still connect them in-process via `connect_staged`.)
#[must_use]
pub fn project_staged_mcp(
    prepared: &PreparedMcpServer,
    trusted: bool,
    relay: Option<&crate::mcp_relay::McpRelay>,
    thread: &str,
) -> awaken_run_executor_acp::McpServerConfig {
    use awaken_run_executor_acp::{McpCredential, McpServerConfig, McpTransport};
    let (url, credential) = match (&prepared.bearer, trusted, relay) {
        // β: a trusted local run may carry the raw bearer inline, dialing the server directly.
        (Some(bearer), true, _) => (
            prepared.url.clone(),
            McpCredential::TrustedInline {
                secret: bearer.expose_secret().to_string(),
            },
        ),
        // α RESOLVED: a sandboxed run dials the host's loopback relay, which injects the real
        // bearer out of the sandbox's address space — the sandbox itself holds no credential.
        (Some(_), false, Some(relay)) => {
            (relay.route_url(thread, &prepared.name), McpCredential::None)
        }
        // α UNRESOLVED (no relay wired): a secretless reference a broker/gateway resolves
        // out-of-band — the raw bearer still never enters the sandbox.
        (Some(_), false, None) => (
            prepared.url.clone(),
            McpCredential::Reference {
                reference: format!("session-mcp:{}", prepared.name),
            },
        ),
        (None, _, _) => (prepared.url.clone(), McpCredential::None),
    };
    McpServerConfig {
        name: prepared.name.clone(),
        transport: McpTransport::Http { url },
        credential,
    }
}

/// The `plugin_config.acp.mcp_servers` value carrying the staged servers for an ACP run,
/// or `None` when there are none. Fail-closed on a serialize error (the ACP CLI then
/// simply gets no MCP servers rather than a corrupt config).
#[must_use]
pub fn acp_mcp_plugin_value(
    staged: &[PreparedMcpServer],
    trusted: bool,
    relay: Option<&crate::mcp_relay::McpRelay>,
    thread: &str,
) -> Option<serde_json::Value> {
    if staged.is_empty() {
        return None;
    }
    let servers: Vec<_> = staged
        .iter()
        .map(|p| project_staged_mcp(p, trusted, relay, thread))
        .collect();
    serde_json::to_value(servers).ok()
}

/// Overlay a session's staged MCP servers into an **ACP** run's config so the ACP CLI
/// executor reads them from `plugin_config.acp.mcp_servers` (D6). A no-op for a native
/// run (those servers are already connected in-process via [`connect_staged`]) or when
/// there are none. The managed session build is the untrusted/sandboxed path, so callers
/// pass `trusted = false` → **α** (secretless reference); the raw bearer never leaves the
/// host. Mutates the transient session snapshot so the plugin config rides the run
/// without changing the durable published snapshot.
///
/// `is_acp` is the caller's authoritative "this run executes on an external ACP CLI"
/// decision — the host's runtime registration (`AcpBackend::is_acp`), NOT the config's
/// `backend_ref`. The managed `server_config` stamps a fixed `backend_ref` ("default")
/// regardless of the selected runtime, so gating on it here would silently skip every
/// managed ACP session; only a native run (`is_acp == false`) is left untouched, since
/// its MCP servers are already the in-process tools connected by `connect_staged`.
#[must_use]
pub fn overlay_acp_mcp(
    mut config: awaken_runtime_contract::snapshot::ExecutableAgentSnapshot,
    staged: &[PreparedMcpServer],
    is_acp: bool,
    trusted: bool,
    relay: Option<&crate::mcp_relay::McpRelay>,
    thread: &str,
) -> awaken_runtime_contract::snapshot::ExecutableAgentSnapshot {
    if staged.is_empty() || !is_acp {
        return config;
    }
    if let Some(value) = acp_mcp_plugin_value(staged, trusted, relay, thread) {
        let acp = config
            .resolved_spec
            .plugin_config
            .entry("acp".to_string())
            .or_insert_with(|| serde_json::json!({}));
        if let Some(obj) = acp.as_object_mut() {
            obj.insert("mcp_servers".to_string(), value);
        }
    }
    config
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
                // The port carries the ref as a neutral string; re-type at the lookup.
                Some(r.secrets.get(&SecretRef(secret_ref.clone())).await.ok()?)
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

#[cfg(test)]
mod alpha_beta_tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_run_executor_acp::McpCredential;

    fn prepared(bearer: Option<&str>) -> PreparedMcpServer {
        PreparedMcpServer {
            name: "gh".into(),
            url: "https://mcp.gh".into(),
            bearer: bearer.map(RedactedString::new),
            refresh: None,
        }
    }

    #[test]
    fn trusted_gets_beta_inline_sandboxed_gets_alpha_reference() {
        let p = prepared(Some("sk-RAW-SECRET"));
        // β: trusted may carry the raw bearer inline (NOT sandbox-safe).
        let t = project_staged_mcp(&p, true, None, "");
        assert!(matches!(t.credential, McpCredential::TrustedInline { .. }));
        assert!(!t.is_sandbox_safe());
        // α (no relay): a sandboxed run gets a secretless reference (sandbox-safe).
        let s = project_staged_mcp(&p, false, None, "");
        assert!(matches!(s.credential, McpCredential::Reference { .. }));
        assert!(s.is_sandbox_safe());
        // No bearer → None.
        assert!(matches!(
            project_staged_mcp(&prepared(None), false, None, "").credential,
            McpCredential::None
        ));
    }

    #[test]
    fn the_plugin_value_never_carries_a_raw_secret_for_a_sandboxed_run() {
        assert!(acp_mcp_plugin_value(&[], false, None, "").is_none());
        let v = acp_mcp_plugin_value(&[prepared(Some("sk-RAW-SECRET"))], false, None, "").unwrap();
        assert!(v.is_array());
        assert!(
            !v.to_string().contains("sk-RAW-SECRET"),
            "a sandboxed projection must never serialize the raw bearer"
        );
    }

    #[tokio::test]
    async fn a_relay_resolves_alpha_to_a_loopback_url_with_no_sandbox_credential() {
        let relay = crate::mcp_relay::McpRelay::start().await.unwrap();
        let p = prepared(Some("sk-RAW-SECRET"));
        relay.set_routes("t1", std::slice::from_ref(&p));
        // Sandboxed + relay: the projected server dials the relay (loopback), holds NO
        // credential (the relay injects the real bearer host-side), never the raw secret.
        let s = project_staged_mcp(&p, false, Some(&relay), "t1");
        assert!(matches!(s.credential, McpCredential::None));
        assert!(s.is_sandbox_safe());
        let url = match &s.transport {
            awaken_run_executor_acp::McpTransport::Http { url } => url.clone(),
            other => panic!("expected http transport, got {other:?}"),
        };
        assert!(
            url.starts_with("http://127.0.0.1:"),
            "dials the loopback relay: {url}"
        );
        assert!(url.ends_with("/t1/gh"), "routed by thread+name: {url}");
        assert!(!serde_json::to_string(&s).unwrap().contains("sk-RAW-SECRET"));
    }

    #[test]
    fn overlay_injects_into_an_acp_run_and_leaves_a_native_run_untouched() {
        use awaken_runtime_contract::resolved::ModelBinding;
        use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

        // ACP run (is_acp = true) → the servers land under plugin_config.acp.mcp_servers,
        // secretless. The backend_ref is the fixed managed "default" — proving the overlay
        // keys off the caller's runtime decision, not backend_ref.
        let acp = ExecutableAgentSnapshot::builder("a")
            .model(ModelBinding::new("default", "m", "default"))
            .build();
        let out = overlay_acp_mcp(
            acp,
            &[prepared(Some("sk-RAW-SECRET"))],
            true,
            false,
            None,
            "",
        );
        let pc = &out.resolved_spec.plugin_config;
        assert!(pc["acp"]["mcp_servers"].is_array());
        assert!(!serde_json::to_string(pc).unwrap().contains("sk-RAW-SECRET"));

        // Native run (is_acp = false) → untouched (its MCP servers are already in-process
        // tools), even though its backend_ref happens to read "acp:claude".
        let native = ExecutableAgentSnapshot::builder("b")
            .model(ModelBinding::new("p", "m", "acp:claude"))
            .build();
        let out2 = overlay_acp_mcp(native, &[prepared(Some("sk"))], false, false, None, "");
        assert!(!out2.resolved_spec.plugin_config.contains_key("acp"));
    }
}
