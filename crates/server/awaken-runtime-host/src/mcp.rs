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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::repo::{
    CredentialMaterialPatch, CredentialRepo, rotate_credential_materials_exact,
};
use awaken_credential_vault::{
    CredentialSourceId, CredentialStatus, OAUTH_CLIENT_SECRET_SLOT, OAUTH_REFRESH_TOKEN_SLOT,
    SecretRef, SecretStore,
};
use awaken_ext_mcp::{AuthChallenge, Credential, CredentialRefresher, HttpTransportBuilder};
use awaken_protocol_managed::{McpProbe, McpProbeStatus};
use awaken_runtime_contract::plugin::Plugin;
use awaken_runtime_contract::{CredentialRefreshAccess, TokenEndpointAuth};
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
/// projected inline: ACP receives an exact-generation loopback route and the
/// Worker retains plaintext.
pub(crate) fn project_mcp_transport(
    prepared: &McpTransportMaterial,
    generation: &awaken_protocol_managed::McpGenerationRef,
    relay: Option<&crate::mcp_relay::McpRelay>,
) -> Result<awaken_run_executor_acp::McpServerConfig, HostError> {
    use awaken_run_executor_acp::{McpServerConfig, McpTransport};
    let url = match (&prepared.bearer, relay) {
        // A sandboxed run dials the host's loopback relay, which injects the real
        // bearer out of the sandbox's address space — the sandbox itself holds no credential.
        (Some(_), Some(relay)) => relay.route_url(generation).ok_or_else(|| {
            HostError::internal(format!(
                "authenticated MCP generation {}:{} has no staged relay capability",
                generation.attachment_id.0, generation.generation.0
            ))
        })?,
        // There is no ACP-side credential resolver. Returning
        // the original URL plus a placeholder would report false success and send
        // an unusable bearer to the target. Fail closed until an explicit mediated
        // endpoint has actually been realized.
        (Some(_), None) => {
            return Err(HostError::internal(format!(
                "authenticated MCP generation {}:{} requires the Worker relay",
                generation.attachment_id.0, generation.generation.0
            )));
        }
        (None, _) => prepared.url.clone(),
    };
    Ok(McpServerConfig {
        name: prepared.name.clone(),
        transport: McpTransport::Http { url },
    })
}

/// The refresh half of a prepared MCP server (an `mcp_oauth` vault credential
/// entered with a refresh object — public or confidential client): everything
/// [`VaultRefresher`] needs to run the `refresh_token` grant and reseal the
/// results. Carries [`SecretRef`]s plus the [`SecretStore`] handle — never
/// secret material.
#[derive(Clone)]
pub(crate) struct McpRefreshMaterial {
    credential_id: CredentialSourceId,
    /// The same exact, fingerprinted execution fact persisted on the MCP
    /// generation. Runtime does not project it into a second refresh DTO.
    access: CredentialRefreshAccess,
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
}

impl McpRefreshMaterial {
    pub(crate) fn new(
        credential_id: CredentialSourceId,
        access: CredentialRefreshAccess,
        credentials: Arc<dyn CredentialRepo>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        Self {
            credential_id,
            access,
            credentials,
            secrets,
        }
    }
}

/// The host-side [`CredentialRefresher`] of the managed vault design (ADR-0043):
/// consulted by the ext-mcp HTTP transport once per auth challenge. It performs
/// an RFC 6749 `refresh_token` grant against the credential's stored token
/// endpoint — `POST` form-encoded `grant_type=refresh_token&refresh_token=…`
/// (+`scope`/`resource` when configured), with client authentication per the
/// stored [`TokenEndpointAuth`]:
/// - `none` (public client): `client_id=…` in the form body;
/// - `client_secret_basic`: `Authorization: Basic
///   base64(urlencode(client_id):urlencode(client_secret))` (RFC 6749 §2.3.1),
///   and the `client_id` stays OUT of the form body;
/// - `client_secret_post`: `client_id=…&client_secret=…` in the form body.
///
/// The confidential-client secret is read from the vault at refresh time; a
/// missing/unreadable sealed secret refuses the exchange (`None`). On success it
/// publishes the new access token and any rotated refresh token as one higher
/// exact credential revision, then hands the transport the fresh bearer to
/// retry with. ANY failure (network, non-2xx, malformed JSON, a lifecycle error)
/// returns `None`, so the transport surfaces the original challenge — fail
/// closed, never a panic.
pub struct VaultRefresher {
    credential_id: CredentialSourceId,
    access: tokio::sync::Mutex<CredentialRefreshAccess>,
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
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
    pub fn new(
        credential_id: CredentialSourceId,
        access: CredentialRefreshAccess,
        credentials: Arc<dyn CredentialRepo>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        let http = http_client_for(&access.token_endpoint);
        Self {
            credential_id,
            access: tokio::sync::Mutex::new(access),
            credentials,
            secrets,
            http,
        }
    }

    fn from_material(refresh: McpRefreshMaterial) -> Self {
        Self::new(
            refresh.credential_id,
            refresh.access,
            refresh.credentials,
            refresh.secrets,
        )
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
        // Serialize refreshes for this transport. The guarded value advances to
        // the newly committed exact revision after every successful exchange.
        let mut access = self.access.lock().await;
        if !access.has_valid_client_authentication_binding()
            || !access.has_valid_configuration_fingerprint()
        {
            return None;
        }
        let source = self.credentials.get(&self.credential_id).await.ok()?;
        if source.status != CredentialStatus::Active
            || u64::try_from(source.version).ok()? != access.credential_revision
            || source.material_ref.as_ref().map(|reference| &reference.0)
                != Some(&access.access_token_ref)
            || source
                .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)
                .map(|reference| &reference.0)
                != Some(&access.refresh_token_ref)
            || access.client_secret_ref.as_ref()
                != source
                    .auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
                    .map(|reference| &reference.0)
        {
            return None;
        }
        let refresh_token = self
            .secrets
            .get(&SecretRef(access.refresh_token_ref.clone()))
            .await
            .ok()?;
        // A confidential scheme's sealed client secret is read HERE, per
        // exchange; missing/unreadable → None (fail closed: the transport
        // surfaces the original challenge).
        let client_secret = match access.token_endpoint_auth {
            TokenEndpointAuth::ClientSecretBasic | TokenEndpointAuth::ClientSecretPost => {
                let secret_ref = access.client_secret_ref.as_ref()?;
                Some(
                    self.secrets
                        .get(&SecretRef(secret_ref.clone()))
                        .await
                        .ok()?,
                )
            }
            TokenEndpointAuth::None => None,
        };
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.expose_secret()),
        ];
        let mut request = self.http.post(&access.token_endpoint);
        match access.token_endpoint_auth {
            // Public client: the bare client_id rides in the form body.
            TokenEndpointAuth::None => form.push(("client_id", access.client_id.as_str())),
            // RFC 6749 §2.3.1: HTTP Basic with the form-urlencoded credential
            // pair; the client_id is OMITTED from the form body.
            TokenEndpointAuth::ClientSecretBasic => {
                request = request.header(
                    reqwest::header::AUTHORIZATION,
                    basic_client_auth(&access.client_id, client_secret.as_ref()?.expose_secret()),
                );
            }
            // RFC 6749 §2.3.1 form alternative: client_id + client_secret in
            // the body.
            TokenEndpointAuth::ClientSecretPost => {
                form.push(("client_id", access.client_id.as_str()));
                form.push(("client_secret", client_secret.as_ref()?.expose_secret()));
            }
        }
        if let Some(scope) = &access.scope {
            form.push(("scope", scope.as_str()));
        }
        if let Some(resource) = &access.resource {
            form.push(("resource", resource.as_str()));
        }
        let response = request.form(&form).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response.json().await.ok()?;
        let access_token = body.get("access_token")?.as_str()?.to_string();
        let mut auxiliary = BTreeMap::new();
        if let Some(rotated) = body.get("refresh_token").and_then(|v| v.as_str()) {
            auxiliary.insert(
                OAUTH_REFRESH_TOKEN_SLOT.to_string(),
                Some(RedactedString::new(rotated.to_string())),
            );
        }
        let rotated = rotate_credential_materials_exact(
            &self.credential_id,
            i64::try_from(access.credential_revision).ok()?,
            CredentialMaterialPatch {
                primary: Some(RedactedString::new(access_token.clone())),
                auxiliary,
            },
            self.secrets.as_ref(),
            self.credentials.as_ref(),
        )
        .await
        .ok()?;
        let access_token_ref = rotated.material_ref.as_ref()?.0.clone();
        let refresh_token_ref = rotated
            .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)?
            .0
            .clone();
        let client_secret_ref = rotated
            .auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
            .map(|reference| reference.0.clone());
        *access = CredentialRefreshAccess::new(
            u64::try_from(rotated.version).ok()?,
            access.token_endpoint.clone(),
            access.client_id.clone(),
            access.token_endpoint_auth,
            client_secret_ref,
            refresh_token_ref,
            access_token_ref,
            access.scope.clone(),
            access.resource.clone(),
        );
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

    /// Extend one already-active exact generation without reopening credential
    /// material or reconnecting MCP. The immutable realization binding must be
    /// identical and only the expiry/idempotency attempt may advance.
    pub(crate) fn renew_mcp_projection(
        &self,
        request: &awaken_protocol_managed::StageMcpAttachment,
    ) -> Result<Option<awaken_protocol_managed::McpRealizationReceipt>, HostError> {
        let binding = request.renewal_binding_fingerprint();
        self.session_slots
            .update(&request.generation.session_id, |slot| {
                let Some(projection) = slot.mcp.iter_mut().find(|projection| {
                    projection.generation.session_id == request.generation.session_id
                        && projection.generation.attachment_id == request.generation.attachment_id
                        && projection.generation.generation == request.generation.generation
                        && projection.generation.runtime_incarnation
                            == request.generation.runtime_incarnation
                        && projection.generation.lease_epoch == request.generation.lease_epoch
                }) else {
                    return Ok(None);
                };
                if projection.state != crate::session_slot::McpProjectionState::Active
                    || projection.realization_id != request.realization_id
                    || projection.renewal_binding_fingerprint != binding
                    || request.generation.lease_expires_at_unix_ms
                        <= projection.generation.lease_expires_at_unix_ms
                {
                    return Err(HostError::internal(
                        "MCP lease renewal conflicts with the active realization",
                    ));
                }
                let receipt = awaken_protocol_managed::McpRealizationReceipt {
                    generation: request.generation.clone(),
                    realization_id: request.realization_id.clone(),
                    selected_plaintext_holder: request.selected_plaintext_holder.clone(),
                    actual_realization_kind: projection.receipt.actual_realization_kind,
                    receipt_fingerprint: request.fingerprint(),
                };
                projection.generation = request.generation.clone();
                projection.stage_idempotency_key = request.stage_idempotency_key.clone();
                projection.receipt = receipt.clone();
                Ok(Some(receipt))
            })
    }

    pub(crate) async fn publish_mcp_projection(
        &self,
        generation: &awaken_protocol_managed::McpGenerationRef,
    ) -> Result<(), HostError> {
        if let Some(projection) = self.mcp_projection(generation) {
            if matches!(
                projection.state,
                crate::session_slot::McpProjectionState::Draining
                    | crate::session_slot::McpProjectionState::Removed
            ) {
                return Err(HostError::internal(
                    "non-visible MCP generation cannot be published",
                ));
            }
            if let Some(server) = &projection.server
                && server.bearer.is_some()
                && projection.native_wiring.is_none()
            {
                let relay = self.mcp_relay.get().ok_or_else(|| {
                    HostError::internal("authenticated MCP generation has no staged relay")
                })?;
                if !relay.update_staged_route(generation, server) {
                    return Err(HostError::internal(
                        "authenticated MCP generation has no exact staged relay route",
                    ));
                }
            }
        }
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
            Some(result) => {
                result?;
                Ok(())
            }
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
            builder = builder.refresher(Arc::new(VaultRefresher::from_material(refresh.clone()))
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
mod acp_projection_tests {
    use super::*;
    use awaken_agent_contract::RedactedString;

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
        // Without a live relay, fail closed: this process has no alternate
        // reference resolver and may not report a non-functional projection.
        assert!(project_mcp_transport(&p, &generation, None).is_err());
        // Anonymous access remains a direct secret-free route.
        assert!(project_mcp_transport(&prepared(None), &generation, None).is_ok());
    }

    #[tokio::test]
    async fn a_relay_projects_a_loopback_url_with_no_sandbox_credential() {
        let relay = crate::mcp_relay::McpRelay::start().await.unwrap();
        let p = prepared(Some("sk-RAW-SECRET"));
        let generation = generation();
        relay.set_route(&generation, &p);
        // Sandboxed + relay: the projected server dials the relay (loopback), holds NO
        // credential (the relay injects the real bearer host-side), never the raw secret.
        let s = project_mcp_transport(&p, &generation, Some(&relay)).unwrap();
        assert!(!format!("{s:?}").contains("sk-RAW-SECRET"));
        let url = match &s.transport {
            awaken_run_executor_acp::McpTransport::Http { url } => url.clone(),
            other => panic!("expected http transport, got {other:?}"),
        };
        assert!(
            url.starts_with("http://127.0.0.1:"),
            "dials the loopback relay: {url}"
        );
        assert!(
            url.contains("/t1/mcp-gh/3/")
                && url
                    .rsplit('/')
                    .next()
                    .is_some_and(|token| token.len() == 32),
            "routed by exact generation: {url}"
        );
        assert!(!serde_json::to_string(&s).unwrap().contains("sk-RAW-SECRET"));
    }

    /// Runtime's final refresh gate follows the contract decision table too:
    /// valid authoring fingerprint + unchanged facts may proceed; changing any
    /// executable fact after compilation must stop before secret lookup/network.
    #[tokio::test]
    async fn oauth_refresh_fails_closed_when_exact_configuration_is_tampered() {
        let mut access = awaken_runtime_contract::CredentialRefreshAccess::new(
            1,
            "https://auth.example/token".into(),
            "client".into(),
            awaken_runtime_contract::TokenEndpointAuth::None,
            None,
            "refresh-ref".into(),
            "access-ref".into(),
            None,
            None,
        );
        access.scope = Some("tampered".into());
        let secrets: Arc<dyn awaken_credential_vault::SecretStore> =
            Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        let credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo> =
            Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let refresher = VaultRefresher::new(
            CredentialSourceId("cred:test".into()),
            access,
            credentials,
            secrets,
        );
        assert_eq!(
            refresher
                .refresh(&AuthChallenge {
                    status: 401,
                    www_authenticate: None,
                })
                .await,
            None
        );
    }
}
