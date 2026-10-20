// @formal entrypoint credential.mcp_adopted_revocation_removes_accepting_generation_before_new_calls
// @formal entrypoint credential.mcp_client_admission_requires_exact_delivery_adapter_and_http_transport
// @formal entrypoint credential.mcp_delivery_mode_is_frozen_and_never_falls_back
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
//! `awaken_session_configuration_contract::McpProbe` port the `mcp_oauth_validate` route
//! drives — a connect + `initialize` handshake as the live credential check).

use crate::mcp_material::{McpTransportMaterial, McpTransportMaterialKind, McpWiring};
use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::RedactedString;
use awaken_ext_mcp::McpToolTransport;
use awaken_ext_mcp::{Credential, HttpTransportBuilder, McpRequestTrace};
use awaken_ext_skills::SkillRegistry as _;
use awaken_runtime_contract::plugin::Plugin;
use awaken_session_configuration_contract::{McpProbe, McpProbeStatus, ProbeHttpResponse};

mod attachment;
mod credential_target;
mod error;
mod input;
mod prompt_skills;
mod sessions;

pub use attachment::{McpCredentialMaterialization, McpStagingSession};
pub use error::{McpError, McpErrorKind};
pub use input::{McpInputHost, McpInputRecipients};
pub use sessions::{McpGenerationAuthority, McpQuiescenceProof, McpSessions, SessionMcpStates};

/// Native Worker-side MCP calls may legitimately outlive control-plane
/// discovery. This policy is deliberately private runtime composition, not
/// attachment desired state or a public Managed Agents wire extension.
const MATERIALIZED_MCP_TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(300);

/// Project private Runtime material to ACP configuration. Worker-held material
/// becomes an exact-generation loopback route; an explicitly admitted
/// workload-held credential crosses only the process-local ACP Session field.
pub fn project_mcp_session_transport(
    prepared: &McpTransportMaterial,
    request: &awaken_session_configuration_contract::mcp::StageMcpAttachment,
    relay: Option<&crate::mcp_relay::McpRelay>,
    receipt: &awaken_session_configuration_contract::mcp::McpRealizationReceipt,
    adapter_policy: &dyn awaken_acp_contract::AcpAdapterPolicy<
        awaken_runtime_contract::resolved::ProviderServerTool,
    >,
    adapter_id: &str,
) -> Result<awaken_acp_contract::SessionMcpServer, McpError> {
    let generation = &request.generation;
    if let McpTransportMaterialKind::SandboxStdio { command, args } = &prepared.transport {
        if receipt.actual_realization_kind
            == Some(awaken_credential_contract::CredentialRealizationKind::ProcessProtocolField)
        {
            return Err(McpError::internal(
                "process-private MCP credentials require an HTTP Session transport",
            ));
        }
        receipt.verify(request).map_err(|_| {
            McpError::internal("MCP realization receipt does not match its exact stage request")
        })?;
        return Ok(awaken_acp_contract::SessionMcpServer {
            name: prepared.name.clone(),
            command: Some(command.clone()),
            args: args.clone(),
            url: None,
            auth: None,
        });
    }
    let (original_url, bearer, _) = prepared.http().expect("HTTP material checked above");
    let process_protocol_field = receipt.actual_realization_kind
        == Some(awaken_credential_contract::CredentialRealizationKind::ProcessProtocolField);
    if process_protocol_field {
        if bearer.is_none() {
            return Err(McpError::internal(
                "process-private MCP credential projection has no material",
            ));
        }
        receipt
            .verify_for_delivery(
                request,
                awaken_credential_contract::McpCredentialDelivery::ClientInjection,
                awaken_credential_contract::CredentialRealizationKind::ProcessProtocolField,
            )
            .map_err(|_| {
                McpError::internal(
                    "MCP client-injection receipt does not match its exact stage request",
                )
            })?;
    } else if receipt.actual_realization_kind
        == Some(awaken_credential_contract::CredentialRealizationKind::PlatformRelay)
    {
        receipt
            .verify_for_delivery(
                request,
                awaken_credential_contract::McpCredentialDelivery::GatewayMediation,
                awaken_credential_contract::CredentialRealizationKind::PlatformRelay,
            )
            .map_err(|_| {
                McpError::internal(
                    "MCP gateway-mediation receipt does not match its exact stage request",
                )
            })?;
    } else {
        receipt.verify(request).map_err(|_| {
            McpError::internal("MCP realization receipt does not match its exact stage request")
        })?;
    }
    let (url, auth) = match bearer {
        Some(bearer) if process_protocol_field => {
            let admitted = adapter_policy.admits_mcp_client_credential(
                adapter_id,
                Some(awaken_credential_contract::McpCredentialDelivery::ClientInjection),
                true,
            );
            if !admitted {
                return Err(McpError::internal(
                    "ACP adapter does not admit process-private MCP client injection",
                ));
            }
            (
                original_url.to_string(),
                Some((
                    "Authorization".to_string(),
                    format!("Bearer {}", bearer.expose_secret()),
                )),
            )
        }
        // A sandboxed run dials the host's loopback relay, which injects the real
        // bearer out of the sandbox's address space — the sandbox itself holds no credential.
        Some(_)
            if receipt.actual_realization_kind
                == Some(awaken_credential_contract::CredentialRealizationKind::WorkerRelay)
                || receipt.actual_realization_kind.is_none() =>
        {
            // `None` is the persisted pre-discriminator receipt. Exact request
            // verification plus an already-staged generation route preserves
            // its WorkerRelay behavior without permitting client injection.
            let relay = relay.ok_or_else(|| {
                McpError::internal(format!(
                    "authenticated MCP generation {}:{} requires the Worker relay",
                    generation.attachment_id().0,
                    generation.generation().version()
                ))
            })?;
            (
                relay.route_url(generation).ok_or_else(|| {
                    McpError::internal(format!(
                        "authenticated MCP generation {}:{} has no staged relay capability",
                        generation.attachment_id().0,
                        generation.generation().version()
                    ))
                })?,
                None,
            )
        }
        // There is no ACP-side credential resolver. Returning
        // the original URL plus a placeholder would report false success and send
        // an unusable bearer to the target. Fail closed until an explicit mediated
        // endpoint has actually been realized.
        Some(_) => {
            return Err(McpError::internal(format!(
                "authenticated MCP generation {}:{} has no admitted delivery",
                generation.attachment_id().0,
                generation.generation().version()
            )));
        }
        // A Platform-mediated generation already dials the mediator: staging
        // put the mediator's route here and resolved no material at all. Say so
        // explicitly rather than letting it share the anonymous arm, so the two
        // stay distinguishable.
        None if receipt.actual_realization_kind
            == Some(awaken_credential_contract::CredentialRealizationKind::PlatformRelay) =>
        {
            (original_url.to_string(), None)
        }
        None => (original_url.to_string(), None),
    };
    Ok(awaken_acp_contract::SessionMcpServer {
        name: prepared.name.clone(),
        command: None,
        args: Vec::new(),
        url: Some(url),
        auth,
    })
}

/// The live MCP credential probe (ADR-0043), implementing the
/// [`awaken_session_configuration_contract::McpProbe`] port over `awaken-ext-mcp`: connect +
/// MCP `initialize` handshake + `tools/list` with the materialized bearer.
/// Completion → `Valid`; a final auth challenge (401/403) → `Invalid` with the
/// HTTP status; anything else (unreachable, protocol or server error, timeout)
/// → `Unknown`. Every non-completion also reports the failing JSON-RPC method
/// and, when the server answered, its response with the bearer scrubbed. This
/// is the only place the MCP client is named for validation — the adapter
/// crate stays wire-client-free.
pub struct ExtMcpProbe;

/// The whole probe, including a slow failed-response body, is bounded.
const MCP_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[async_trait::async_trait]
impl McpProbe for ExtMcpProbe {
    async fn probe(&self, url: &str, bearer: &RedactedString) -> McpProbeStatus {
        self.inspect(url, Some(bearer)).await.status
    }
    async fn inspect(
        &self,
        url: &str,
        bearer: Option<&RedactedString>,
    ) -> awaken_session_configuration_contract::McpInspection {
        use awaken_session_configuration_contract::{McpInspection, McpProbeFailure, McpProbeTool};
        let trace = Arc::new(McpRequestTrace::new());
        let mut builder =
            HttpTransportBuilder::new(url.to_string()).request_trace(Arc::clone(&trace));
        if let Some(bearer) = bearer {
            builder = builder.credential(Credential::Bearer(bearer.expose_secret().to_string()));
        }
        let result = tokio::time::timeout(MCP_PROBE_TIMEOUT, async {
            let transport = builder.connect().await?;
            transport.list_tools().await
        })
        .await;
        if let Ok(Ok(tools)) = result {
            return McpInspection {
                status: McpProbeStatus::Valid,
                tools: tools
                    .into_iter()
                    .map(|tool| McpProbeTool {
                        name: tool.name,
                        description: tool.description,
                        input_schema: tool.input_schema,
                    })
                    .collect(),
                failure: None,
            };
        }
        let captured = trace.last_failure();
        let status = match &captured {
            Some(failure) if failure.status == 401 || failure.status == 403 => {
                McpProbeStatus::Invalid {
                    http_status: failure.status,
                }
            }
            _ => McpProbeStatus::Unknown,
        };
        let secrets: Vec<&str> = bearer
            .map(RedactedString::expose_secret)
            .into_iter()
            .collect();
        McpInspection {
            status,
            tools: Vec::new(),
            failure: Some(McpProbeFailure {
                // No request attempted means the handshake never started.
                method: trace
                    .last_method()
                    .unwrap_or_else(|| "initialize".to_owned()),
                http_response: captured.map(|failure| {
                    ProbeHttpResponse::scrubbed(
                        failure.status,
                        failure.content_type.as_deref(),
                        &failure.body,
                        failure.body_truncated,
                        &secrets,
                    )
                }),
            }),
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
) -> Result<McpWiring, McpError> {
    connect_materialized_with_tool_call_timeout(staged, MATERIALIZED_MCP_TOOL_CALL_TIMEOUT).await
}

async fn connect_materialized_with_tool_call_timeout(
    staged: &[McpTransportMaterial],
    tool_call_timeout: Duration,
) -> Result<McpWiring, McpError> {
    let mut wiring = McpWiring::empty();
    for server in staged {
        let Some((url, bearer, refresh)) = server.http() else {
            return Err(McpError::internal(format!(
                "sandbox stdio MCP server `{}` requires an attached sandbox stream",
                server.name
            )));
        };
        let credential = match bearer {
            Some(token) => awaken_ext_mcp::Credential::Bearer(token.expose_secret().to_string()),
            None => awaken_ext_mcp::Credential::None,
        };
        let builder = HttpTransportBuilder::new(url.to_string())
            .credential(credential)
            .tool_call_timeout(tool_call_timeout);
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
) -> McpError {
    use awaken_ext_mcp::McpTransportError;

    let message = format!("mcp server `{server_name}` at {target}: {error}");
    match error {
        McpTransportError::ServerError(ref detail) if detail.starts_with("auth challenge:") => {
            McpError::classified("mcp_authentication_failed", message)
        }
        McpTransportError::TransportError(_) | McpTransportError::Timeout(_) => {
            McpError::unavailable_classified("mcp_connection_failed", message)
        }
        _ => McpError::classified("mcp_protocol_failed", message),
    }
}

/// Bind a sandbox-attached MCP process to the Native Runtime. Process ownership
/// remains with the Session projection; this function owns only the MCP client
/// protocol layered over its duplex channel.
pub(crate) async fn connect_sandbox_stdio(
    server: &McpTransportMaterial,
    channel: Box<dyn awaken_agent_channel::AgentChannel>,
) -> Result<McpWiring, McpError> {
    if !matches!(
        server.transport,
        McpTransportMaterialKind::SandboxStdio { .. }
    ) {
        return Err(McpError::internal(
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
        McpError::internal(format!(
            "sandbox stdio mcp server `{}`: {error}",
            server.name
        ))
    })?;
    let transport = transport.tool_call_timeout(MATERIALIZED_MCP_TOOL_CALL_TIMEOUT);
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
) -> Result<(), McpError> {
    let (transport, call_fence) = awaken_ext_mcp::transport::revocable_transport(transport);
    let connected =
        awaken_ext_mcp::McpServer::start(&server.name, Arc::clone(&transport), list_changed)
            .await
            .map_err(|e| McpError::internal(format!("mcp server `{}`: {e}", server.name)))?;
    if server.prompts_as_skills {
        match prompt_skills::McpPromptSkillRegistry::discover(&server.name, Arc::clone(&transport))
            .await
        {
            Ok(registry) if !registry.list().map_err(McpError::internal)?.is_empty() => {
                wiring.skill_registries.push(Arc::new(registry));
            }
            Ok(_) => {}
            Err(error) => return Err(McpError::internal(error)),
        }
    }
    let plugin = connected.plugin();
    let contributions = plugin.resolve();
    wiring.tool_ids.extend(
        contributions
            .dynamic_tools
            .iter()
            .map(|tool| tool.executable().id().to_string()),
    );
    wiring.plugins.push(Arc::new(plugin));
    wiring
        .notification_transports
        .push((server.name.clone(), transport));
    wiring.call_fences.push(call_fence);
    Ok(())
}

#[cfg(test)]
mod acp_projection_tests {
    use super::*;

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

    fn generation() -> awaken_session_configuration_contract::mcp::McpGenerationRef {
        crate::test_mcp::mcp_fence("t1", "mcp-gh", 3, "runtime-1", 2, u64::MAX)
    }

    fn request(
        boundary: awaken_agent_contract::credential::PlaintextBoundary,
    ) -> awaken_session_configuration_contract::mcp::StageMcpAttachment {
        let holder =
            awaken_agent_contract::credential::PlaintextHolder::new(boundary, "test-holder");
        awaken_session_configuration_contract::mcp::StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: generation(),
            realization_id: "realize-gh".into(),
            stage_idempotency_key: "stage-gh".into(),
            name: "gh".into(),
            target: awaken_agent_contract::McpTarget::parse_http("https://mcp.gh").unwrap(),
            prompts_as_skills: false,
            credential: Some(awaken_credential_contract::CredentialAccess::new(
                awaken_credential_contract::CredentialRef {
                    id: "credential-gh".into(),
                    revision: 2,
                },
                awaken_credential_contract::CredentialMaterialSource::ControlPlaneReference,
                awaken_credential_contract::CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                awaken_credential_contract::CredentialExecutionPolicy::exact(
                    holder.clone(),
                    awaken_credential_contract::ModelExposurePolicy::VirtualOnly,
                ),
            )),
            selected_plaintext_holder: Some(holder),
        }
    }

    fn receipt(
        request: &awaken_session_configuration_contract::mcp::StageMcpAttachment,
        realization: awaken_credential_contract::CredentialRealizationKind,
    ) -> awaken_session_configuration_contract::mcp::McpRealizationReceipt {
        awaken_session_configuration_contract::mcp::McpRealizationReceipt {
            generation: request.generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: request.selected_plaintext_holder.clone(),
            actual_realization_kind: Some(realization),
            receipt_fingerprint: request.fingerprint(),
        }
    }

    fn legacy_receipt(
        request: &awaken_session_configuration_contract::mcp::StageMcpAttachment,
    ) -> awaken_session_configuration_contract::mcp::McpRealizationReceipt {
        awaken_session_configuration_contract::mcp::McpRealizationReceipt {
            generation: request.generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: request.selected_plaintext_holder.clone(),
            actual_realization_kind: None,
            receipt_fingerprint: request.fingerprint(),
        }
    }

    #[test]
    fn client_injection_uses_only_the_exact_process_protocol_field() {
        use awaken_agent_contract::credential::PlaintextBoundary;
        use awaken_credential_contract::CredentialRealizationKind;
        // Cause/effect rules: P1 declared Claude/Codex + exact HTTP process-field
        // receipt -> private redacted auth; P2 another sink or tampered receipt
        // -> refusal for either declared client; P3 undeclared OpenCode ->
        // refusal. All clients use the same projection and receipt validator.
        let prepared = prepared(Some("raw-secret"));
        let exact_request = request(PlaintextBoundary::Workload);
        let exact = receipt(
            &exact_request,
            CredentialRealizationKind::ProcessProtocolField,
        );
        for backend in ["claude", "codex"] {
            let projected = project_mcp_session_transport(
                &prepared,
                &exact_request,
                None,
                &exact,
                awaken_run_executor_acp::acp_adapter_policy(),
                backend,
            )
            .expect("P1 exact ClientInjection");
            assert_eq!(projected.url.as_deref(), Some("https://mcp.gh"));
            assert_eq!(
                projected
                    .auth
                    .as_ref()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
                Some(("Authorization", "Bearer raw-secret"))
            );
            assert!(!format!("{projected:?}").contains("raw-secret"));
            for (boundary, realization) in [
                (
                    PlaintextBoundary::Workload,
                    CredentialRealizationKind::ProcessSecretEnvironment,
                ),
                (
                    PlaintextBoundary::Workload,
                    CredentialRealizationKind::PrivateSecretFile,
                ),
                (
                    PlaintextBoundary::Platform,
                    CredentialRealizationKind::PlatformRelay,
                ),
            ] {
                let rejected_request = request(boundary);
                let rejected = receipt(&rejected_request, realization);
                assert!(
                    project_mcp_session_transport(
                        &prepared,
                        &rejected_request,
                        None,
                        &rejected,
                        awaken_run_executor_acp::acp_adapter_policy(),
                        backend,
                    )
                    .is_err(),
                    "P2 alternate sink: {backend}"
                );
            }
            let mut tampered = exact.clone();
            tampered.receipt_fingerprint = "different-stage".into();
            assert!(
                project_mcp_session_transport(
                    &prepared,
                    &exact_request,
                    None,
                    &tampered,
                    awaken_run_executor_acp::acp_adapter_policy(),
                    backend
                )
                .is_err(),
                "P2 tampered receipt: {backend}"
            );
        }
        assert!(
            project_mcp_session_transport(
                &prepared,
                &exact_request,
                None,
                &exact,
                awaken_run_executor_acp::acp_adapter_policy(),
                "opencode",
            )
            .is_err()
        );
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
                McpErrorKind::Internal,
                "mcp_authentication_failed",
            ),
            (
                "R2",
                McpTransportError::TransportError("connection refused".into()),
                McpErrorKind::Unavailable,
                "mcp_connection_failed",
            ),
            (
                "R3",
                McpTransportError::Timeout("30s".into()),
                McpErrorKind::Unavailable,
                "mcp_connection_failed",
            ),
            (
                "R4",
                McpTransportError::ProtocolError("bad frame".into()),
                McpErrorKind::Internal,
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
        let request = request(awaken_agent_contract::credential::PlaintextBoundary::Worker);
        let receipt = legacy_receipt(&request);
        // Without a live relay, fail closed: this process has no alternate
        // reference resolver and may not report a non-functional projection.
        assert!(
            project_mcp_session_transport(
                &p,
                &request,
                None,
                &receipt,
                awaken_run_executor_acp::acp_adapter_policy(),
                "codex"
            )
            .is_err()
        );
        // Anonymous access remains a direct secret-free route.
        let mut anonymous = request;
        anonymous.credential = None;
        anonymous.selected_plaintext_holder = None;
        let receipt = legacy_receipt(&anonymous);
        assert!(
            project_mcp_session_transport(
                &prepared(None),
                &anonymous,
                None,
                &receipt,
                awaken_run_executor_acp::acp_adapter_policy(),
                "codex"
            )
            .is_ok()
        );
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
        let mut request = request(awaken_agent_contract::credential::PlaintextBoundary::Worker);
        request.name = "playwright".into();
        request.target = awaken_agent_contract::McpTarget::sandbox_stdio(
            "playwright-mcp",
            vec!["--headless".into()],
        )
        .unwrap();
        request.credential = None;
        request.selected_plaintext_holder = None;
        let receipt = legacy_receipt(&request);
        let projected = project_mcp_session_transport(
            &prepared,
            &request,
            None,
            &receipt,
            awaken_run_executor_acp::acp_adapter_policy(),
            "codex",
        )
        .unwrap();
        assert_eq!(projected.command.as_deref(), Some("playwright-mcp"));
        assert_eq!(projected.args, ["--headless"]);
        assert!(projected.url.is_none());
    }

    #[tokio::test]
    async fn a_relay_projects_a_loopback_url_with_no_sandbox_credential() {
        let relay = crate::mcp_relay::McpRelay::start().await.unwrap();
        let p = prepared(Some("sk-RAW-SECRET"));
        let request = request(awaken_agent_contract::credential::PlaintextBoundary::Worker);
        let receipt = legacy_receipt(&request);
        relay.set_route(&request.generation, &p);
        // Sandboxed + relay: the projected server dials the relay (loopback), holds NO
        // credential (the relay injects the real bearer host-side), never the raw secret.
        let s = project_mcp_session_transport(
            &p,
            &request,
            Some(&relay),
            &receipt,
            awaken_run_executor_acp::acp_adapter_policy(),
            "codex",
        )
        .unwrap();
        assert!(!format!("{s:?}").contains("sk-RAW-SECRET"));
        assert!(s.auth.is_none());
        let url = s.url.expect("expected HTTP transport");
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
    }
}

#[cfg(test)]
mod native_mcp_wiring_tests {
    use super::*;

    struct GatedSandboxTool {
        started: tokio::sync::Notify,
        returned: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl awaken_ext_mcp::jsonrpc::ServerRequestHandler for GatedSandboxTool {
        async fn handle(
            &self,
            _id: &serde_json::Value,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<serde_json::Value, awaken_ext_mcp::jsonrpc::ServerRequestError> {
            Ok(match method {
                "initialize" => {
                    serde_json::json!({"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"sandbox","version":"1"}})
                }
                "tools/list" => {
                    serde_json::json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]})
                }
                "tools/call" => {
                    self.started.notify_one();
                    self.returned.notified().await;
                    serde_json::json!({"content":[{"type":"text","text":"returned"}]})
                }
                _ => {
                    return Err(
                        awaken_ext_mcp::jsonrpc::ServerRequestError::method_not_found(method),
                    );
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sandbox_stdio_wiring_uses_the_same_materialized_tool_budget_as_http() {
        // S1 sandbox process is already attached; S2 initialize/discovery returns;
        // S3 tools/call crosses the shared 30s control deadline but returns before
        // the one 300s materialized tool deadline => original result, no retry.
        // S4 still pending past 300s => Indeterminate, never pretend termination.
        // The production constructor and canonical Peer exchange real duplex
        // frames; virtual time exercises both boundary outcomes without sleeps.
        for expires in [false, true] {
            let (client, server) = tokio::io::duplex(64 * 1024);
            let handler = Arc::new(GatedSandboxTool {
                started: tokio::sync::Notify::new(),
                returned: tokio::sync::Notify::new(),
            });
            let (reader, writer) = tokio::io::split(server);
            let (_peer, _notifications) =
                awaken_ext_mcp::jsonrpc::JsonRpcPeer::new(reader, writer, Some(handler.clone()));
            let staged = McpTransportMaterial {
                name: "sandbox".into(),
                prompts_as_skills: false,
                transport: McpTransportMaterialKind::SandboxStdio {
                    command: "unused-owner-launch".into(),
                    args: vec![],
                },
            };
            let wiring = connect_sandbox_stdio(&staged, Box::new(client))
                .await
                .unwrap();
            let tools = wiring.plugins[0].resolve();
            let executable = tools.dynamic_tools[0].executable();
            let mut call = Box::pin(executable.invoke(awaken_tool_contract::tool::ToolCall {
                call_id: "same-call".into(),
                tool_id: "mcp__sandbox__echo".into(),
                arguments: serde_json::json!({}),
            }));
            tokio::select! { result=&mut call=>panic!("returned before server: {result:?}"), _=handler.started.notified()=>{} }
            tokio::time::advance(Duration::from_secs(if expires { 301 } else { 31 })).await;
            if expires {
                assert!(matches!(
                    call.await,
                    Err(awaken_tool_contract::tool::ToolError::Indeterminate(_))
                ));
            } else {
                tokio::select! {biased; result=&mut call=>panic!("stdio kept the control deadline: {result:?}"), _=tokio::task::yield_now()=>{} }
                handler.returned.notify_one();
                let output = call.await.unwrap();
                assert_eq!(output.call_id, "same-call");
                assert_eq!(output.text(), "returned");
            }
        }
    }

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
    async fn materialized_http_wiring_applies_the_tool_call_timeout_override() {
        // Causes: C1 Runtime Host materializes native HTTP MCP; C2 initialize and
        // tools/list finish normally; C3 tools/call exceeds the injected tool
        // deadline. Effects: E1 staging succeeds through the unchanged control
        // deadline; E2 the discovered executable preserves typed Indeterminate
        // (timeout cannot prove provider termination or safe model reentry);
        // E3 upstream observes exactly one tools/call. Rule H1 C1+C2+C3 ->
        // E1+E2+E3. Constraint: production calls the same helper with the private
        // 300s policy; neither attachment desired state nor probe wiring carries
        // this override. FMECA: omitting the host builder override silently leaves
        // materialized calls on the shared 30s default.
        assert_eq!(MATERIALIZED_MCP_TOOL_CALL_TIMEOUT, Duration::from_secs(300));
        let (url, seen) =
            crate::test_mcp::start_with_tool_call_delay(Duration::from_millis(100)).await;
        let wiring = connect_materialized_with_tool_call_timeout(
            &[material(url, false)],
            Duration::from_millis(25),
        )
        .await
        .expect("H1/E1: control-plane staging succeeds");
        let contributions = wiring.plugins[0].resolve();
        let error = contributions.dynamic_tools[0]
            .executable()
            .invoke(awaken_tool_contract::tool::ToolCall {
                call_id: "call-1".into(),
                tool_id: "mcp__docs__echo".into(),
                arguments: serde_json::json!({ "value": "slow" }),
            })
            .await
            .expect_err("H1/E2: injected tool deadline is wired to execution");
        assert!(
            matches!(
                &error,
                awaken_tool_contract::tool::ToolError::Indeterminate(_)
            ),
            "{error}"
        );
        let tool_calls = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "tools/call")
            .count();
        assert_eq!(tool_calls, 1, "H1/E3: transport never replays");
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
        assert_eq!(registry.list().unwrap()[0].id, "mcp:docs:review");
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
