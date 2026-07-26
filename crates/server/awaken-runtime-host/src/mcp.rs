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
use awaken_runtime_contract::plugin::Plugin;
use base64::Engine as _;

use crate::host::HostError;

/// Private Worker-side material for one exact MCP generation. It is never an
/// authoring or desired-state value and cannot cross the Runtime Host boundary.
#[derive(Clone)]
pub(crate) struct McpTransportMaterial {
    pub name: String,
    pub url: String,
    pub bearer: Option<awaken_agent_contract::RedactedString>,
    /// The vault credential's refresh configuration, when it has one: the
    /// connect then registers a [`VaultRefresher`] so an expired access token
    /// is exchanged mid-request instead of failing the turn.
    pub refresh: Option<McpRefreshMaterial>,
}

/// Project private Worker material to ACP configuration. A real bearer is never
/// projected inline: ACP receives an exact-generation loopback route or an
/// unresolved opaque reference and the Worker retains plaintext.
#[must_use]
pub(crate) fn project_mcp_transport(
    prepared: &McpTransportMaterial,
    generation: &awaken_protocol_managed::McpGenerationRef,
    relay: Option<&crate::mcp_relay::McpRelay>,
) -> awaken_run_executor_acp::McpServerConfig {
    use awaken_run_executor_acp::{McpCredential, McpServerConfig, McpTransport};
    let (url, credential) = match (&prepared.bearer, relay) {
        // α RESOLVED: a sandboxed run dials the host's loopback relay, which injects the real
        // bearer out of the sandbox's address space — the sandbox itself holds no credential.
        (Some(_), Some(relay)) => (relay.route_url(generation), McpCredential::None),
        // α UNRESOLVED (no relay wired): a secretless reference a broker/gateway resolves
        // out-of-band — the raw bearer still never enters the sandbox.
        (Some(_), None) => (
            prepared.url.clone(),
            McpCredential::Reference {
                reference: format!("session-mcp:{}", prepared.name),
            },
        ),
        (None, _) => (prepared.url.clone(), McpCredential::None),
    };
    McpServerConfig {
        name: prepared.name.clone(),
        transport: McpTransport::Http { url },
        credential,
    }
}

/// The refresh half of a prepared MCP server (an `mcp_oauth` vault credential
/// entered with a refresh object — public or confidential client): everything
/// [`VaultRefresher`] needs to run the `refresh_token` grant and reseal the
/// results. Carries [`SecretRef`]s plus the [`SecretStore`] handle — never
/// secret material.
#[derive(Clone)]
pub struct McpRefreshMaterial {
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
    refresh: McpRefreshMaterial,
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
    pub fn new(refresh: McpRefreshMaterial) -> Self {
        let http = http_client_for(&refresh.token_endpoint);
        Self { refresh, http }
    }
}

fn http_client_for(url: &str) -> reqwest::Client {
    let mut builder = reqwest::Client::builder();
    if reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
    {
        builder = builder.no_proxy();
    }
    builder.build().expect("build OAuth HTTP client")
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

/// The discovered surface of one thread's staged MCP servers. Each server is
/// represented by the canonical live [`awaken_ext_mcp::McpPlugin`], so model
/// descriptors and executable tools have one source of truth. The initial exact
/// ids are also retained for the Session permission gate.
#[derive(Clone)]
pub(crate) struct McpWiring {
    pub plugins: Vec<Arc<dyn Plugin>>,
    pub tool_ids: Vec<String>,
}

impl crate::SharedHost {
    pub(crate) fn active_mcp_projections(
        &self,
        thread: &str,
    ) -> Vec<crate::session_slot::McpGenerationProjection> {
        self.session_slots
            .read(thread, |slot| {
                slot.mcp
                    .iter()
                    .filter(|projection| {
                        projection.state == crate::session_slot::McpProjectionState::Active
                            && projection.server.is_some()
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn mcp_projection(
        &self,
        generation: &awaken_protocol_managed::McpGenerationRef,
    ) -> Option<crate::session_slot::McpGenerationProjection> {
        self.session_slots
            .read(&generation.session_id, |slot| {
                slot.mcp
                    .iter()
                    .find(|projection| projection.generation == *generation)
                    .cloned()
            })
            .flatten()
    }

    pub(crate) fn insert_mcp_projection(
        &self,
        projection: crate::session_slot::McpGenerationProjection,
    ) -> Result<(), HostError> {
        let thread = projection.generation.session_id.clone();
        self.session_slots.update(&thread, |slot| {
            if slot
                .mcp
                .iter()
                .any(|existing| existing.generation == projection.generation)
            {
                return Err(HostError::internal(
                    "MCP generation projection already exists with another realization",
                ));
            }
            slot.mcp.push(projection);
            Ok(())
        })
    }

    pub(crate) async fn publish_mcp_projection(
        &self,
        generation: &awaken_protocol_managed::McpGenerationRef,
    ) -> Result<(), HostError> {
        let changed = self.session_slots.modify(&generation.session_id, |slot| {
            let Some(index) = slot
                .mcp
                .iter()
                .position(|projection| projection.generation == *generation)
            else {
                return Err(HostError::internal("unknown MCP generation projection"));
            };
            match slot.mcp[index].state {
                crate::session_slot::McpProjectionState::Staged => {
                    for projection in &mut slot.mcp {
                        if projection.generation.attachment_id == generation.attachment_id
                            && projection.state == crate::session_slot::McpProjectionState::Active
                        {
                            projection.state = crate::session_slot::McpProjectionState::Draining;
                        }
                    }
                    slot.mcp[index].state = crate::session_slot::McpProjectionState::Active;
                    slot.runtime = None;
                    Ok(true)
                }
                crate::session_slot::McpProjectionState::Active => Ok(false),
                crate::session_slot::McpProjectionState::Draining
                | crate::session_slot::McpProjectionState::Removed => Err(HostError::internal(
                    "non-visible MCP generation cannot be published",
                )),
            }
        });
        match changed {
            Some(result) => result.map(|_| ()),
            None => Err(HostError::internal("unknown MCP Session projection")),
        }
    }

    pub(crate) async fn drain_mcp_projection(
        &self,
        generation: &awaken_protocol_managed::McpGenerationRef,
    ) -> Result<(), HostError> {
        let result = self.session_slots.modify(&generation.session_id, |slot| {
            let Some(projection) = slot
                .mcp
                .iter_mut()
                .find(|projection| projection.generation == *generation)
            else {
                // Cleanup is an idempotent exact-generation command. A fresh
                // Runtime incarnation legitimately has no process-local copy of
                // an already fenced durable Draining generation.
                return Ok(());
            };
            projection.state = crate::session_slot::McpProjectionState::Removed;
            projection.server = None;
            projection.native_wiring = None;
            slot.runtime = None;
            Ok(())
        });
        if let Some(relay) = self.mcp_relay.get() {
            relay.remove_route(generation);
        }
        result.unwrap_or(Ok(()))
    }
}

impl McpWiring {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
            tool_ids: Vec::new(),
        }
    }
}

/// Connect every staged server and collect the discovered tools. Fail closed:
/// a configured server that cannot connect (network, 401, bad wire) fails the
/// build — never a silent skip — naming the server so the error is actionable.
/// A server with refresh configuration gets a [`VaultRefresher`], so an expired
/// access token is exchanged mid-connect (and mid-turn) instead of failing.
pub(crate) async fn connect_materialized(
    staged: &[McpTransportMaterial],
) -> Result<McpWiring, HostError> {
    let mut wiring = McpWiring::empty();
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
        let transport = builder.connect_streaming().await.map_err(|e| {
            HostError::internal(format!(
                "mcp server `{}` at {}: {e}",
                server.name, server.url
            ))
        })?;
        let connected = awaken_ext_mcp::McpServer::connect_http(&server.name, transport)
            .await
            .map_err(|e| HostError::internal(format!("mcp server `{}`: {e}", server.name)))?;
        let plugin = connected.plugin();
        let contributions = plugin.resolve();
        wiring.tool_ids.extend(
            contributions
                .dynamic_tools
                .iter()
                .map(|tool| tool.tool.id().to_string()),
        );
        wiring.plugins.push(Arc::new(plugin));
    }
    Ok(wiring)
}

#[cfg(test)]
mod alpha_beta_tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_run_executor_acp::McpCredential;

    fn prepared(bearer: Option<&str>) -> McpTransportMaterial {
        McpTransportMaterial {
            name: "gh".into(),
            url: "https://mcp.gh".into(),
            bearer: bearer.map(RedactedString::new),
            refresh: None,
        }
    }

    fn generation() -> awaken_protocol_managed::McpGenerationRef {
        awaken_protocol_managed::McpGenerationRef {
            session_id: "t1".into(),
            attachment_id: awaken_protocol_managed::McpAttachmentId("mcp-gh".into()),
            generation: awaken_protocol_managed::McpGeneration(3),
            runtime_incarnation: "runtime-1".into(),
            lease_epoch: 2,
            lease_expires_at_unix_ms: u64::MAX,
        }
    }

    #[test]
    fn acp_projection_never_contains_the_real_bearer() {
        let p = prepared(Some("sk-RAW-SECRET"));
        let generation = generation();
        // Without a live relay, fail closed to an opaque reference; there is no
        // trusted-inline compatibility branch.
        let s = project_mcp_transport(&p, &generation, None);
        assert!(matches!(s.credential, McpCredential::Reference { .. }));
        assert!(s.is_sandbox_safe());
        assert!(!serde_json::to_string(&s).unwrap().contains("sk-RAW-SECRET"));
        // No bearer → None.
        assert!(matches!(
            project_mcp_transport(&prepared(None), &generation, None).credential,
            McpCredential::None
        ));
    }

    #[tokio::test]
    async fn a_relay_resolves_alpha_to_a_loopback_url_with_no_sandbox_credential() {
        let relay = crate::mcp_relay::McpRelay::start().await.unwrap();
        let p = prepared(Some("sk-RAW-SECRET"));
        let generation = generation();
        relay.set_route(&generation, &p);
        // Sandboxed + relay: the projected server dials the relay (loopback), holds NO
        // credential (the relay injects the real bearer host-side), never the raw secret.
        let s = project_mcp_transport(&p, &generation, Some(&relay));
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
        assert!(
            url.ends_with("/t1/mcp-gh/3"),
            "routed by exact generation: {url}"
        );
        assert!(!serde_json::to_string(&s).unwrap().contains("sk-RAW-SECRET"));
    }
}
