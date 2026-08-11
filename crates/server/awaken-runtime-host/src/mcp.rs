//! MCP runtime services for the shared Host (ADR-0043 Phase 3).
//!
//! Owns the wire-client half of per-thread MCP wiring: given the servers a
//! session staged (already credential-materialized), connect each through
//! `awaken-ext-mcp` and hand back the executable tools + model-visible
//! descriptors the host registers on that thread's runtime. Split out of
//! `host.rs` so the host keeps session orchestration and this module owns the
//! outbound wire encoding.
//!
//! It also owns [`ExtMcpProbe`] (the
//! `awaken_session_contract::McpProbe` port the `mcp_oauth_validate` route
//! drives — a connect + `initialize` handshake as the live credential check).

use std::sync::{Arc, Mutex};

use awaken_agent_contract::RedactedString;
use awaken_ext_mcp::{AuthChallenge, Credential, CredentialRefresher, HttpTransportBuilder};
use awaken_ext_skills::SkillRegistry as _;
use awaken_runtime_contract::plugin::Plugin;
use awaken_session_contract::{McpProbe, McpProbeStatus};

use crate::host::HostError;

mod prompt_skills;

/// Private Worker-side material for one exact MCP generation. It is never an
/// authoring or desired-state value and cannot cross the Runtime Host boundary.
#[derive(Clone)]
pub(crate) struct McpTransportMaterial {
    pub name: String,
    pub prompts_as_skills: bool,
    pub transport: McpTransportMaterialKind,
}

#[derive(Clone)]
pub(crate) enum McpTransportMaterialKind {
    Http {
        url: String,
        bearer: Option<awaken_agent_contract::RedactedString>,
        refresh: Option<Box<McpRefreshMaterial>>,
    },
    SandboxStdio {
        command: String,
        args: Vec<String>,
    },
}

type HttpTransportMaterialRef<'a> = (
    &'a str,
    &'a Option<awaken_agent_contract::RedactedString>,
    &'a Option<Box<McpRefreshMaterial>>,
);

impl McpTransportMaterial {
    fn http(&self) -> Option<HttpTransportMaterialRef<'_>> {
        match &self.transport {
            McpTransportMaterialKind::Http {
                url,
                bearer,
                refresh,
            } => Some((url, bearer, refresh)),
            McpTransportMaterialKind::SandboxStdio { .. } => None,
        }
    }

    pub(crate) fn bearer(&self) -> Option<&awaken_agent_contract::RedactedString> {
        self.http().and_then(|(_, bearer, _)| bearer.as_ref())
    }

    pub(crate) fn http_url(&self) -> Option<&str> {
        self.http().map(|(url, _, _)| url)
    }

    pub(crate) fn refresh(&self) -> Option<Box<McpRefreshMaterial>> {
        self.http().and_then(|(_, _, refresh)| refresh.clone())
    }
}

/// Project private Worker material to ACP configuration. A real bearer is never
/// projected inline: ACP receives an exact-generation loopback route and the
/// Worker retains plaintext.
pub(crate) fn project_mcp_transport(
    prepared: &McpTransportMaterial,
    generation: &awaken_session_contract::McpGenerationRef,
    relay: Option<&crate::mcp_relay::McpRelay>,
) -> Result<awaken_run_executor_acp::McpServerConfig, HostError> {
    use awaken_run_executor_acp::{McpServerConfig, McpTransport};
    if let McpTransportMaterialKind::SandboxStdio { command, args } = &prepared.transport {
        return Ok(McpServerConfig {
            name: prepared.name.clone(),
            transport: McpTransport::Stdio {
                command: command.clone(),
                args: args.clone(),
            },
        });
    }
    let (original_url, bearer, _) = prepared.http().expect("HTTP material checked above");
    let url = match (bearer, relay) {
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
        (None, _) => original_url.to_string(),
    };
    Ok(McpServerConfig {
        name: prepared.name.clone(),
        transport: McpTransport::Http { url },
    })
}

/// The already-authorized refresh half of one prepared MCP server. Coordinator
/// authority owns the Vault and constructs this transport adapter; Runtime only
/// invokes the neutral refresh contract after an auth challenge.
#[derive(Clone)]
pub(crate) struct McpRefreshMaterial(pub(crate) Arc<dyn CredentialRefresher>);

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
/// [`awaken_session_contract::McpProbe`] port over `awaken-ext-mcp`: connect +
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
    pub skill_registries: Vec<Arc<dyn awaken_ext_skills::SkillRegistry>>,
}

impl crate::SharedHost {
    pub(crate) async fn stop_session_mcp_processes(&self, thread: &str) {
        let processes = self
            .session_slots
            .modify(thread, |slot| {
                slot.mcp
                    .iter_mut()
                    .filter_map(|projection| projection.mcp_process.take())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for process in processes {
            let _ = process
                .signal(awaken_provisioning_contract::Signal::Term)
                .await;
            let _ = process.wait().await;
        }
    }

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
        generation: &awaken_session_contract::McpGenerationRef,
    ) -> Option<crate::session_slot::McpGenerationProjection> {
        self.session_slots
            .read(&generation.session_id, |slot| {
                slot.mcp
                    .iter()
                    .find(|projection| projection.request.generation == *generation)
                    .cloned()
            })
            .flatten()
    }

    pub(crate) fn insert_mcp_projection(
        &self,
        projection: crate::session_slot::McpGenerationProjection,
    ) -> Result<(), HostError> {
        let thread = projection.request.generation.session_id.clone();
        self.session_slots.update(&thread, |slot| {
            if slot
                .mcp
                .iter()
                .any(|existing| existing.request.generation == projection.request.generation)
            {
                return Err(HostError::internal(
                    "MCP generation projection already exists with another realization",
                ));
            }
            slot.mcp.push(projection);
            Ok(())
        })
    }

    /// Extend one already-staged or active exact generation without reopening
    /// credential material or reconnecting MCP. The immutable realization
    /// binding must be identical and only the expiry/idempotency attempt may
    /// advance. Supporting Staged closes the race where Control renews while the
    /// first stage effect is in flight and republishes its longer exact fence.
    pub(crate) fn renew_mcp_projection(
        &self,
        request: &awaken_session_contract::StageMcpAttachment,
    ) -> Result<Option<awaken_session_contract::McpRealizationReceipt>, HostError> {
        let binding = request.renewal_binding_fingerprint();
        self.session_slots
            .update(&request.generation.session_id, |slot| {
                let Some(projection) = slot.mcp.iter_mut().find(|projection| {
                    projection.request.generation.session_id == request.generation.session_id
                        && projection.request.generation.attachment_id
                            == request.generation.attachment_id
                        && projection.request.generation.generation == request.generation.generation
                        && projection.request.generation.runtime_incarnation
                            == request.generation.runtime_incarnation
                        && projection.request.generation.lease_epoch
                            == request.generation.lease_epoch
                }) else {
                    return Ok(None);
                };
                if !matches!(
                    projection.state,
                    crate::session_slot::McpProjectionState::Staged
                        | crate::session_slot::McpProjectionState::Active
                ) || projection.request.realization_id != request.realization_id
                    || projection.request.renewal_binding_fingerprint() != binding
                    || request.generation.lease_expires_at_unix_ms
                        <= projection.request.generation.lease_expires_at_unix_ms
                {
                    return Err(HostError::internal(
                        "MCP lease renewal conflicts with the staged realization",
                    ));
                }
                let receipt = awaken_session_contract::McpRealizationReceipt {
                    generation: request.generation.clone(),
                    realization_id: request.realization_id.clone(),
                    selected_plaintext_holder: request.selected_plaintext_holder.clone(),
                    actual_realization_kind: projection.receipt.actual_realization_kind,
                    receipt_fingerprint: request.fingerprint(),
                };
                projection.request = request.clone();
                projection.receipt = receipt.clone();
                Ok(Some(receipt))
            })
    }

    /// Forget an already-cleaned process-local tombstone only when Control
    /// replays the exact same authorized Stage command. A removed projection
    /// owns no remaining process, route, or credential material, so retaining
    /// it must not permanently prevent the canonical realization driver from
    /// recovering after a concurrent Control CAS. Any changed request remains
    /// resident and is rejected by the caller as a conflicting realization.
    pub(crate) fn forget_exact_removed_mcp_projection(
        &self,
        request: &awaken_session_contract::StageMcpAttachment,
    ) -> bool {
        let request_fingerprint = request.fingerprint();
        self.session_slots
            .modify(&request.generation.session_id, |slot| {
                let Some(index) = slot.mcp.iter().position(|projection| {
                    projection.request.generation == request.generation
                        && projection.state == crate::session_slot::McpProjectionState::Removed
                        && projection.request.realization_id == request.realization_id
                        && projection.request.stage_idempotency_key == request.stage_idempotency_key
                        && projection.receipt.receipt_fingerprint == request_fingerprint
                }) else {
                    return false;
                };
                slot.mcp.remove(index);
                true
            })
            .unwrap_or(false)
    }

    pub(crate) async fn publish_mcp_projection(
        &self,
        generation: &awaken_session_contract::McpGenerationRef,
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
                && server.bearer().is_some()
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
                .position(|projection| projection.request.generation == *generation)
            else {
                return Err(HostError::internal("unknown MCP generation projection"));
            };
            match slot.mcp[index].state {
                crate::session_slot::McpProjectionState::Staged => {
                    for projection in &mut slot.mcp {
                        if projection.request.generation.attachment_id == generation.attachment_id
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
        generation: &awaken_session_contract::McpGenerationRef,
    ) -> Result<(), HostError> {
        let result = self.session_slots.modify(&generation.session_id, |slot| {
            let Some(projection) = slot
                .mcp
                .iter_mut()
                .find(|projection| projection.request.generation == *generation)
            else {
                // Cleanup is an idempotent exact-generation command. A fresh
                // Runtime incarnation legitimately has no process-local copy of
                // an already fenced durable Draining generation.
                return Ok(None);
            };
            projection.state = crate::session_slot::McpProjectionState::Removed;
            projection.server = None;
            projection.native_wiring = None;
            let process = projection.mcp_process.take();
            slot.runtime = None;
            Ok(process)
        });
        if let Some(Ok(Some(process))) = &result {
            let _ = process
                .signal(awaken_provisioning_contract::Signal::Term)
                .await;
            let _ = process.wait().await;
        }
        if let Some(relay) = self.mcp_relay.get() {
            relay.remove_route(generation);
        }
        result.map(|result| result.map(|_| ())).unwrap_or(Ok(()))
    }
}

impl McpWiring {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
            tool_ids: Vec::new(),
            skill_registries: Vec::new(),
        }
    }
}

/// Connect every staged server and collect the discovered tools. Fail closed:
/// a configured server that cannot connect (network, 401, bad wire) fails the
/// build — never a silent skip — naming the server so the error is actionable.
/// A server with refresh configuration receives the Coordinator-built refresher,
/// so an expired token can be exchanged without giving Runtime a Vault handle.
pub(crate) async fn connect_materialized(
    staged: &[McpTransportMaterial],
) -> Result<McpWiring, HostError> {
    let mut wiring = McpWiring::empty();
    for server in staged {
        let Some((url, bearer, refresh)) = server.http() else {
            return Err(HostError::internal(format!(
                "sandbox stdio MCP server `{}` requires an attached sandbox stream",
                server.name
            )));
        };
        let credential = match bearer {
            Some(token) => awaken_ext_mcp::Credential::Bearer(token.expose_secret().to_string()),
            None => awaken_ext_mcp::Credential::None,
        };
        let builder = HttpTransportBuilder::new(url.to_string()).credential(credential);
        let builder = match refresh {
            Some(refresh) => builder.refresher(refresh.0.clone()),
            None => builder,
        };
        let transport = builder
            .connect_streaming()
            .await
            .map_err(|error| mcp_transport_error(&server.name, url, error))?;
        let list_changed = transport.subscribe_list_changed();
        let transport: Arc<dyn awaken_ext_mcp::transport::McpToolTransport> = Arc::new(transport);
        append_connected(&mut wiring, server, transport, list_changed).await?;
    }
    Ok(wiring)
}

fn mcp_transport_error(
    server_name: &str,
    target: &str,
    error: awaken_ext_mcp::McpTransportError,
) -> HostError {
    use awaken_ext_mcp::McpTransportError;

    let message = format!("mcp server `{server_name}` at {target}: {error}");
    match error {
        McpTransportError::ServerError(ref detail) if detail.starts_with("auth challenge:") => {
            HostError::classified("mcp_authentication_failed", message)
        }
        McpTransportError::TransportError(_) | McpTransportError::Timeout(_) => {
            HostError::unavailable_classified("mcp_connection_failed", message)
        }
        _ => HostError::classified("mcp_protocol_failed", message),
    }
}

/// Bind a sandbox-attached MCP process to the Native Runtime. Process ownership
/// remains with the Session projection; this function owns only the MCP client
/// protocol layered over its duplex channel.
pub(crate) async fn connect_sandbox_stdio(
    server: &McpTransportMaterial,
    channel: Box<dyn awaken_run_executor_acp::AgentChannelType>,
) -> Result<McpWiring, HostError> {
    if !matches!(
        server.transport,
        McpTransportMaterialKind::SandboxStdio { .. }
    ) {
        return Err(HostError::internal(
            "attached MCP stream requires sandbox stdio material",
        ));
    }
    let transport = awaken_ext_mcp::StdioTransport::connect_stream(
        channel,
        None,
        awaken_ext_mcp::DEFAULT_TIMEOUT,
        None,
    )
    .await
    .map_err(|error| {
        HostError::internal(format!(
            "sandbox stdio mcp server `{}`: {error}",
            server.name
        ))
    })?;
    let list_changed = transport.subscribe_list_changed();
    let transport: Arc<dyn awaken_ext_mcp::transport::McpToolTransport> = Arc::new(transport);
    let mut wiring = McpWiring::empty();
    append_connected(&mut wiring, server, transport, list_changed).await?;
    Ok(wiring)
}

async fn append_connected(
    wiring: &mut McpWiring,
    server: &McpTransportMaterial,
    transport: Arc<dyn awaken_ext_mcp::transport::McpToolTransport>,
    list_changed: tokio::sync::broadcast::Receiver<awaken_ext_mcp::ListChangedKind>,
) -> Result<(), HostError> {
    let connected =
        awaken_ext_mcp::McpServer::start(&server.name, Arc::clone(&transport), list_changed)
            .await
            .map_err(|e| HostError::internal(format!("mcp server `{}`: {e}", server.name)))?;
    if server.prompts_as_skills {
        match prompt_skills::McpPromptSkillRegistry::discover(&server.name, Arc::clone(&transport))
            .await
        {
            Ok(registry) if !registry.list().is_empty() => {
                wiring.skill_registries.push(Arc::new(registry));
            }
            Ok(_) => {}
            Err(error) => return Err(HostError::internal(error)),
        }
    }
    let plugin = connected.plugin();
    let contributions = plugin.resolve();
    wiring.tool_ids.extend(
        contributions
            .dynamic_tools
            .iter()
            .map(|tool| tool.tool.id().to_string()),
    );
    wiring.plugins.push(Arc::new(plugin));
    Ok(())
}

#[cfg(test)]
mod acp_projection_tests {
    use super::*;
    use crate::HostErrorKind;
    use awaken_agent_contract::RedactedString;

    fn prepared(bearer: Option<&str>) -> McpTransportMaterial {
        McpTransportMaterial {
            name: "gh".into(),
            prompts_as_skills: false,
            transport: McpTransportMaterialKind::Http {
                url: "https://mcp.gh".into(),
                bearer: bearer.map(RedactedString::new),
                refresh: None,
            },
        }
    }

    fn generation() -> awaken_session_contract::McpGenerationRef {
        awaken_session_contract::McpGenerationRef {
            session_id: "t1".into(),
            attachment_id: awaken_session_contract::McpAttachmentId("mcp-gh".into()),
            generation: awaken_session_contract::McpGeneration(3),
            runtime_incarnation: "runtime-1".into(),
            lease_epoch: 2,
            lease_expires_at_unix_ms: u64::MAX,
        }
    }

    #[test]
    fn mcp_transport_faults_have_stable_origin_classification() {
        // Cause/effect decision table: R1 auth challenge => permanent classified
        // authentication failure; R2 network transport and R3 timeout => retryable
        // connection failure; R4 protocol/server payload fault => permanent MCP
        // protocol failure. Human-readable server text never chooses the class.
        use awaken_ext_mcp::McpTransportError;

        for (rule, source, kind, code) in [
            (
                "R1",
                McpTransportError::ServerError("auth challenge: bearer".into()),
                HostErrorKind::Internal,
                "mcp_authentication_failed",
            ),
            (
                "R2",
                McpTransportError::TransportError("connection refused".into()),
                HostErrorKind::Unavailable,
                "mcp_connection_failed",
            ),
            (
                "R3",
                McpTransportError::Timeout("30s".into()),
                HostErrorKind::Unavailable,
                "mcp_connection_failed",
            ),
            (
                "R4",
                McpTransportError::ProtocolError("bad frame".into()),
                HostErrorKind::Internal,
                "mcp_protocol_failed",
            ),
        ] {
            let error = mcp_transport_error("docs", "https://mcp.example", source);
            assert_eq!(error.kind, kind, "{rule}");
            assert_eq!(error.code, code, "{rule}");
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

    #[test]
    fn sandbox_stdio_projection_preserves_command_without_an_http_bridge() {
        let prepared = McpTransportMaterial {
            name: "playwright".into(),
            prompts_as_skills: false,
            transport: McpTransportMaterialKind::SandboxStdio {
                command: "playwright-mcp".into(),
                args: vec!["--headless".into()],
            },
        };
        let projected = project_mcp_transport(&prepared, &generation(), None).unwrap();
        assert!(matches!(
            projected.transport,
            awaken_run_executor_acp::McpTransport::Stdio { command, args }
                if command == "playwright-mcp" && args == ["--headless"]
        ));
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
}

#[cfg(test)]
mod prompt_skill_projection_tests {
    use super::*;

    // MCP Prompt Skill cause/effect table:
    // T1 opt-out -> tools only and no prompts/list; T2 opt-in + Native +
    // capability -> metadata catalog now, prompts/get only at activation;
    // T3 missing capability -> fail closed. Parameter/get failures are owned by
    // the MCP registry tests, ACP rejection by host H19, and durable/wire switch
    // behavior by the Session contract tests. Discovery is never activation.
    fn material(url: String, prompts_as_skills: bool) -> McpTransportMaterial {
        McpTransportMaterial {
            name: "docs".into(),
            prompts_as_skills,
            transport: McpTransportMaterialKind::Http {
                url,
                bearer: None,
                refresh: None,
            },
        }
    }

    #[tokio::test]
    async fn disabled_flag_never_discovers_prompts() {
        let (url, seen) = crate::test_mcp::start_with_prompts(None, true).await;
        let wiring = connect_materialized(&[material(url, false)])
            .await
            .expect("tools-only wiring succeeds");
        assert!(wiring.skill_registries.is_empty());
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .all(|(method, _)| method != "prompts/list"),
            "disabled means no prompt discovery side effect"
        );
    }

    #[tokio::test]
    async fn enabled_flag_discovers_and_lazily_activates_one_unified_skill() {
        let (url, seen) = crate::test_mcp::start_with_prompts(None, true).await;
        let wiring = connect_materialized(&[material(url, true)])
            .await
            .expect("prompt-capable wiring succeeds");
        assert_eq!(wiring.skill_registries.len(), 1);
        let registry = &wiring.skill_registries[0];
        assert_eq!(registry.list()[0].id, "mcp:docs:review");
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .all(|(method, _)| method != "prompts/get"),
            "catalog discovery remains metadata-only"
        );

        let skill = registry
            .resolve(
                "mcp:docs:review",
                Some(serde_json::json!({ "focus": "security" })),
            )
            .await
            .expect("remote activation succeeds")
            .expect("skill exists");
        assert_eq!(skill.body, "Review focus: security");
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|(method, _)| method == "prompts/get")
        );
    }

    #[tokio::test]
    async fn enabled_flag_fails_closed_when_server_has_no_prompt_capability() {
        let (url, _seen) = crate::test_mcp::start(None).await;
        let error = match connect_materialized(&[material(url, true)]).await {
            Ok(_) => panic!("unsupported opt-in must fail staging"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("prompts/list"), "{error}");
    }
}
