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
use std::time::Duration;

use awaken_agent_contract::RedactedString;
use awaken_ext_mcp::{AuthChallenge, Credential, CredentialRefresher, HttpTransportBuilder};
use awaken_ext_skills::SkillRegistry as _;
use awaken_runtime_contract::plugin::Plugin;
use awaken_session_contract::{McpProbe, McpProbeStatus};

use crate::host::HostError;

mod prompt_skills;

/// Native Worker-side MCP calls may legitimately outlive control-plane
/// discovery. This policy is deliberately private runtime composition, not
/// attachment desired state or a public Managed Agents wire extension.
const MATERIALIZED_HTTP_MCP_TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(300);
const MCP_GENERATION_CALL_QUIESCENCE_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Project private Runtime material to ACP configuration. Worker-held material
/// becomes an exact-generation loopback route; an explicitly admitted
/// workload-held credential crosses only the process-local ACP Session field.
pub(crate) fn project_mcp_session_transport(
    prepared: &McpTransportMaterial,
    request: &awaken_session_contract::StageMcpAttachment,
    relay: Option<&crate::mcp_relay::McpRelay>,
    receipt: &awaken_session_contract::McpRealizationReceipt,
    adapter: &awaken_run_executor_acp::AcpCli,
) -> Result<awaken_run_executor_acp::SessionMcpServer, HostError> {
    let generation = &request.generation;
    if let McpTransportMaterialKind::SandboxStdio { command, args } = &prepared.transport {
        if receipt.actual_realization_kind
            == Some(awaken_runtime_contract::CredentialRealizationKind::ProcessProtocolField)
        {
            return Err(HostError::internal(
                "process-private MCP credentials require an HTTP Session transport",
            ));
        }
        receipt.verify(request).map_err(|_| {
            HostError::internal("MCP realization receipt does not match its exact stage request")
        })?;
        return Ok(awaken_run_executor_acp::SessionMcpServer {
            name: prepared.name.clone(),
            command: Some(command.clone()),
            args: args.clone(),
            url: None,
            auth: None,
        });
    }
    let (original_url, bearer, _) = prepared.http().expect("HTTP material checked above");
    let process_protocol_field = receipt.actual_realization_kind
        == Some(awaken_runtime_contract::CredentialRealizationKind::ProcessProtocolField);
    if process_protocol_field {
        if bearer.is_none() {
            return Err(HostError::internal(
                "process-private MCP credential projection has no material",
            ));
        }
        receipt
            .verify_for_delivery(
                request,
                awaken_credential_contract::McpCredentialDelivery::ClientInjection,
                awaken_runtime_contract::CredentialRealizationKind::ProcessProtocolField,
            )
            .map_err(|_| {
                HostError::internal(
                    "MCP client-injection receipt does not match its exact stage request",
                )
            })?;
    } else {
        receipt.verify(request).map_err(|_| {
            HostError::internal("MCP realization receipt does not match its exact stage request")
        })?;
    }
    let (url, auth) = match bearer {
        Some(bearer) if process_protocol_field => {
            let admitted = adapter.admits_mcp_client_credential(
                Some(awaken_credential_contract::McpCredentialDelivery::ClientInjection),
                true,
            );
            if !admitted {
                return Err(HostError::internal(
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
                == Some(awaken_runtime_contract::CredentialRealizationKind::WorkerRelay)
                || receipt.actual_realization_kind.is_none() =>
        {
            // `None` is the persisted pre-discriminator receipt. Exact request
            // verification plus an already-staged generation route preserves
            // its WorkerRelay behavior without permitting client injection.
            let relay = relay.ok_or_else(|| {
                HostError::internal(format!(
                    "authenticated MCP generation {}:{} requires the Worker relay",
                    generation.attachment_id.0, generation.generation.0
                ))
            })?;
            (
                relay.route_url(generation).ok_or_else(|| {
                    HostError::internal(format!(
                        "authenticated MCP generation {}:{} has no staged relay capability",
                        generation.attachment_id.0, generation.generation.0
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
            return Err(HostError::internal(format!(
                "authenticated MCP generation {}:{} has no admitted delivery",
                generation.attachment_id.0, generation.generation.0
            )));
        }
        None => (original_url.to_string(), None),
    };
    Ok(awaken_run_executor_acp::SessionMcpServer {
        name: prepared.name.clone(),
        command: None,
        args: Vec::new(),
        url: Some(url),
        auth,
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
    /// Exact-generation local call fences. The durable Session generation is
    /// still authoritative; drain closes these projections before acknowledging
    /// its receipt so cloned Runtime tools cannot continue using old material.
    pub call_fences: Vec<awaken_ext_mcp::transport::McpCallFence>,
}

impl crate::SharedHost {
    pub(crate) fn active_mcp_projections(
        &self,
        thread: &str,
    ) -> Vec<crate::session_slot::McpGenerationProjection> {
        self.session_slots
            .read(thread, |slot| {
                if slot.mcp_quiescence_fence.is_some() {
                    return Vec::new();
                }
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
            if slot.mcp_quiescence_fence.is_some() {
                return Err(HostError::unavailable_classified(
                    "session_environment_quiescing",
                    "MCP staging is closed while the Session Environment quiesces",
                ));
            }
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

    /// Transfer a freshly spawned stdio process into the already-installed
    /// exact-generation owner before any further fallible await.
    pub(crate) fn attach_staging_mcp_process(
        &self,
        generation: &awaken_session_contract::McpGenerationRef,
        process: Arc<dyn awaken_provisioning_contract::ProcessHandle>,
    ) -> Result<bool, HostError> {
        self.session_slots
            .modify(&generation.session_id, |slot| {
                let admitted = slot.mcp_quiescence_fence.is_none();
                let projection = slot
                    .mcp
                    .iter_mut()
                    .find(|projection| projection.request.generation == *generation)
                    .ok_or_else(|| HostError::internal("unknown MCP staging owner"))?;
                if !matches!(
                    projection.state,
                    crate::session_slot::McpProjectionState::Staging
                        | crate::session_slot::McpProjectionState::Draining
                ) || projection.staging.is_none()
                    || projection.mcp_process.is_some()
                {
                    return Err(HostError::internal(
                        "MCP staging owner no longer admits a spawned process",
                    ));
                }
                let connect_admitted = admitted
                    && projection.state == crate::session_slot::McpProjectionState::Staging;
                projection.mcp_process = Some(process);
                Ok(connect_admitted)
            })
            .unwrap_or_else(|| Err(HostError::internal("unknown MCP Session projection")))
    }

    /// Publish only wiring into the private staged projection. Durable publish
    /// remains the separate `Staged -> Active` transition.
    pub(crate) fn complete_staging_mcp_projection(
        &self,
        generation: &awaken_session_contract::McpGenerationRef,
        wiring: crate::mcp::McpWiring,
    ) -> Result<(), HostError> {
        self.session_slots
            .modify(&generation.session_id, |slot| {
                if slot.mcp_quiescence_fence.is_some() {
                    return Err(HostError::unavailable_classified(
                        "session_environment_quiescing",
                        "MCP staging cannot commit while the Session Environment quiesces",
                    ));
                }
                let projection = slot
                    .mcp
                    .iter_mut()
                    .find(|projection| projection.request.generation == *generation)
                    .ok_or_else(|| HostError::internal("unknown MCP staging owner"))?;
                if projection.state != crate::session_slot::McpProjectionState::Staging
                    || projection.mcp_process.is_none()
                {
                    return Err(HostError::internal(
                        "MCP staging owner changed before connection completed",
                    ));
                }
                projection.native_wiring = Some(wiring);
                projection.staging = None;
                projection.state = crate::session_slot::McpProjectionState::Staged;
                Ok(())
            })
            .unwrap_or_else(|| Err(HostError::internal("unknown MCP Session projection")))
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
                if slot.mcp_quiescence_fence.is_some() {
                    return Err(HostError::unavailable_classified(
                        "session_environment_quiescing",
                        "MCP renewal is closed while the Session Environment quiesces",
                    ));
                }
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
                if slot.mcp_quiescence_fence.is_some() {
                    return false;
                }
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
        if !self
            .session_slots
            .mcp_realization_admitted(&generation.session_id)
        {
            return Err(HostError::unavailable_classified(
                "session_environment_quiescing",
                "MCP publication is closed while the Session Environment quiesces",
            ));
        }
        if let Some(projection) = self.mcp_projection(generation) {
            if matches!(
                projection.state,
                crate::session_slot::McpProjectionState::Staging
                    | crate::session_slot::McpProjectionState::Draining
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
                match projection.receipt.actual_realization_kind {
                    // `None` is the pre-receipt-discriminator legacy form. It
                    // remains safe only through the same exact staged relay;
                    // it can never fall through to process injection.
                    Some(awaken_runtime_contract::CredentialRealizationKind::WorkerRelay)
                    | None => {
                        let relay = self.mcp_relay.get().ok_or_else(|| {
                            HostError::internal("authenticated MCP generation has no staged relay")
                        })?;
                        if !relay.update_staged_route(generation, server) {
                            return Err(HostError::internal(
                                "authenticated MCP generation has no exact staged relay route",
                            ));
                        }
                    }
                    Some(
                        awaken_runtime_contract::CredentialRealizationKind::ProcessProtocolField,
                    ) => {}
                    _ => {
                        return Err(HostError::internal(
                            "authenticated MCP generation has no admitted delivery",
                        ));
                    }
                }
            }
        }
        let changed = self.session_slots.modify(&generation.session_id, |slot| {
            if slot.mcp_quiescence_fence.is_some() {
                return Err(HostError::unavailable_classified(
                    "session_environment_quiescing",
                    "MCP publication is closed while the Session Environment quiesces",
                ));
            }
            let Some(index) = slot
                .mcp
                .iter()
                .position(|projection| projection.request.generation == *generation)
            else {
                return Err(HostError::internal("unknown MCP generation projection"));
            };
            match slot.mcp[index].state {
                crate::session_slot::McpProjectionState::Staging => Err(HostError::internal(
                    "MCP generation is still staging and cannot be published",
                )),
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
        let Some(drain) = self
            .session_slots
            .read(&generation.session_id, |slot| {
                slot.mcp
                    .iter()
                    .find(|projection| projection.request.generation == *generation)
                    .map(|projection| projection.drain.clone())
            })
            .flatten()
        else {
            return Ok(());
        };
        // Keep the exact owner installed while awaiting. Cancellation releases
        // only this serialization guard; a retry sees Draining plus the same
        // process/activity and resumes the canonical cleanup.
        let _drain = drain.lock().await;
        let projected = self.session_slots.modify(&generation.session_id, |slot| {
            let Some(projection) = slot
                .mcp
                .iter_mut()
                .find(|projection| projection.request.generation == *generation)
            else {
                // Cleanup is an idempotent exact-generation command. A fresh
                // Runtime incarnation legitimately has no process-local copy of
                // an already fenced durable Draining generation.
                return Ok::<_, HostError>(None);
            };
            if !Arc::ptr_eq(&projection.drain, &drain) {
                return Err(HostError::unavailable_classified(
                    "mcp_generation_owner_changed",
                    "MCP generation projection was replaced while cleanup was waiting",
                ));
            }
            if projection.state == crate::session_slot::McpProjectionState::Removed {
                return Ok::<_, HostError>(None);
            }
            // Visibility closes before cancellation. Existing Runtime clones may
            // still hold the dynamic tools, so their exact transport fences are
            // cancelled and drained below before Removed is acknowledged.
            projection.state = crate::session_slot::McpProjectionState::Draining;
            let call_fences = projection
                .native_wiring
                .as_ref()
                .map(|wiring| wiring.call_fences.clone())
                .unwrap_or_default();
            let process = projection.mcp_process.clone();
            let staging = projection.staging.clone();
            // The canonical Session slot must retain this Runtime owner across
            // cancellation and timeout. Only the outer environment-quiescence
            // transaction clears it after every MCP generation and the Hand
            // have proved quiescent; otherwise a retry could lose the active-run
            // fence and manufacture a Removed proof while the old Run is live.
            let runtime = slot.runtime.clone();
            Ok(Some((call_fences, process, staging, runtime)))
        });
        let Some((mut call_fences, process, staging, runtime)) = projected.transpose()?.flatten()
        else {
            return Ok(());
        };

        // WorkerRelay requests are not Runtime tool calls: Axum clones the
        // exact Route before it awaits the body/upstream/stream. Add that
        // route owner's existing MCP call fence to the same drain barrier.
        // Removing the map entry is not quiescence because an accepted clone
        // can still hold its credential and upstream future.
        if let Some(relay) = self.mcp_relay.get()
            && let Some(fence) = relay.route_call_fence(generation)
        {
            call_fences.push(fence);
        }
        // Poll every exact fence together so all admission closes before a
        // single slow permit can consume the bounded settlement window.
        tokio::time::timeout(
            MCP_GENERATION_CALL_QUIESCENCE_TIMEOUT,
            futures_util::future::join_all(call_fences.iter().map(|fence| fence.close_and_wait())),
        )
        .await
        .map_err(|_| {
            HostError::unavailable_classified(
                "mcp_generation_call_quiescence_timeout",
                "MCP generation drain could not quiesce an accepted MCP call",
            )
        })?;

        let had_staging = staging.is_some();
        if let Some(staging) = staging {
            staging.wait().await;
        }
        // Spawn may have completed after Draining was installed but before the
        // activity barrier closed. The task transfers that process into this
        // same projection; re-read it before reaping so no post-snapshot child
        // can be erased as an unowned handle.
        let process = if process.is_none() && had_staging {
            self.session_slots
                .read(&generation.session_id, |slot| {
                    slot.mcp
                        .iter()
                        .find(|projection| projection.request.generation == *generation)
                        .and_then(|projection| projection.mcp_process.clone())
                })
                .flatten()
        } else {
            process
        };

        if let Some(runtime) = runtime {
            if let Some(token) = runtime
                .cancel
                .lock()
                .expect("cancel mutex poisoned")
                .as_ref()
            {
                token.cancel();
            }
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if runtime
                    .active_run
                    .lock()
                    .expect("active run mutex poisoned")
                    .is_none()
                {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(HostError::unavailable_classified(
                        "mcp_generation_quiescence_timeout",
                        "MCP generation drain could not quiesce the active Session Run",
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        if let Some(process) = process {
            awaken_run_executor_acp::Supervisor::reap(
                process.as_ref(),
                std::time::Duration::from_secs(5),
            )
            .await
            .map_err(|error| {
                HostError::unavailable_classified(
                    "mcp_generation_process_reap_failed",
                    format!("MCP generation process did not terminate: {error}"),
                )
            })?;
        }

        // This is the sole visible-route removal edge. It runs only after the
        // exact route fence, Runtime, staging, and process owners are quiescent;
        // cancellation or any failure above leaves the closed Route installed
        // with the Draining projection so retry retains the same proof owner.
        if let Some(relay) = self.mcp_relay.get() {
            relay.remove_route(generation);
        }

        match self.session_slots.modify(&generation.session_id, |slot| {
            let Some(projection) = slot
                .mcp
                .iter_mut()
                .find(|projection| projection.request.generation == *generation)
            else {
                return Ok(());
            };
            if projection.state == crate::session_slot::McpProjectionState::Removed {
                return Ok(());
            }
            if projection.state != crate::session_slot::McpProjectionState::Draining {
                return Err(HostError::internal(
                    "MCP generation changed while its drain was quiescing",
                ));
            }
            projection.server = None;
            projection.native_wiring = None;
            projection.mcp_process = None;
            projection.staging = None;
            projection.state = crate::session_slot::McpProjectionState::Removed;
            Ok(())
        }) {
            Some(result) => result,
            None => Ok(()),
        }
    }

    /// Drain every exact durable generation requested by the Session owner and
    /// any additional local leak, continuing after individual failures. The
    /// returned proof echoes the durable expected set; local slot contents are
    /// used only to find additional cleanup work and to reject residual effects.
    pub(crate) async fn drain_mcp_projections(
        &self,
        thread: &str,
        expected: &[awaken_session_contract::McpGenerationRef],
    ) -> Result<McpQuiescenceProof, HostError> {
        let mut targets = Vec::with_capacity(expected.len());
        for generation in expected {
            if generation.session_id != thread {
                return Err(HostError::internal(
                    "MCP quiescence generation belongs to another Session",
                ));
            }
            if targets.iter().any(|seen| seen == generation) {
                return Err(HostError::internal(
                    "MCP quiescence expected set contains a duplicate generation",
                ));
            }
            targets.push(generation.clone());
        }
        for generation in self
            .session_slots
            .read(thread, |slot| {
                slot.mcp
                    .iter()
                    .filter(|projection| {
                        projection.state != crate::session_slot::McpProjectionState::Removed
                    })
                    .map(|projection| projection.request.generation.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
        {
            if !targets.iter().any(|seen| seen == &generation) {
                targets.push(generation);
            }
        }

        let mut first_error = None;
        for generation in &targets {
            if let Err(error) = self.drain_mcp_projection(generation).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        let residual = self
            .session_slots
            .read(thread, |slot| {
                slot.mcp.iter().any(|projection| {
                    projection.state != crate::session_slot::McpProjectionState::Removed
                })
            })
            .unwrap_or(false);
        if let Some(error) = first_error {
            return Err(error);
        }
        if residual {
            return Err(HostError::unavailable_classified(
                "mcp_generation_quiescence_incomplete",
                "MCP generation cleanup left a process-local owner unresolved",
            ));
        }
        Ok(McpQuiescenceProof {
            generations: expected.to_vec(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct McpQuiescenceProof {
    pub generations: Vec<awaken_session_contract::McpGenerationRef>,
}

impl McpWiring {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
            tool_ids: Vec::new(),
            skill_registries: Vec::new(),
            call_fences: Vec::new(),
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
    connect_materialized_with_tool_call_timeout(staged, MATERIALIZED_HTTP_MCP_TOOL_CALL_TIMEOUT)
        .await
}

async fn connect_materialized_with_tool_call_timeout(
    staged: &[McpTransportMaterial],
    tool_call_timeout: Duration,
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
    let (transport, call_fence) = awaken_ext_mcp::transport::revocable_transport(transport);
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
            .map(|tool| tool.executable().id().to_string()),
    );
    wiring.plugins.push(Arc::new(plugin));
    wiring.call_fences.push(call_fence);
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

    fn request(
        boundary: awaken_runtime_contract::PlaintextBoundary,
    ) -> awaken_session_contract::StageMcpAttachment {
        let holder = awaken_runtime_contract::PlaintextHolder::new(boundary, "test-holder");
        awaken_session_contract::StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: generation(),
            realization_id: "realize-gh".into(),
            stage_idempotency_key: "stage-gh".into(),
            name: "gh".into(),
            target: awaken_session_contract::McpTarget::parse_http("https://mcp.gh").unwrap(),
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
        request: &awaken_session_contract::StageMcpAttachment,
        realization: awaken_runtime_contract::CredentialRealizationKind,
    ) -> awaken_session_contract::McpRealizationReceipt {
        awaken_session_contract::McpRealizationReceipt {
            generation: request.generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: request.selected_plaintext_holder.clone(),
            actual_realization_kind: Some(realization),
            receipt_fingerprint: request.fingerprint(),
        }
    }

    fn legacy_receipt(
        request: &awaken_session_contract::StageMcpAttachment,
    ) -> awaken_session_contract::McpRealizationReceipt {
        awaken_session_contract::McpRealizationReceipt {
            generation: request.generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: request.selected_plaintext_holder.clone(),
            actual_realization_kind: None,
            receipt_fingerprint: request.fingerprint(),
        }
    }

    #[test]
    fn client_injection_uses_only_the_exact_process_protocol_field() {
        use awaken_runtime_contract::{CredentialRealizationKind, PlaintextBoundary};
        let prepared = prepared(Some("raw-secret"));
        let exact_request = request(PlaintextBoundary::Workload);
        let exact = receipt(
            &exact_request,
            CredentialRealizationKind::ProcessProtocolField,
        );
        let projected = project_mcp_session_transport(
            &prepared,
            &exact_request,
            None,
            &exact,
            awaken_run_executor_acp::acp_cli("claude").unwrap(),
        )
        .expect("exact ClientInjection");
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
                    awaken_run_executor_acp::acp_cli("claude").unwrap(),
                )
                .is_err()
            );
        }
        assert!(
            project_mcp_session_transport(
                &prepared,
                &exact_request,
                None,
                &exact,
                awaken_run_executor_acp::acp_cli("codex").unwrap(),
            )
            .is_err()
        );
        let mut tampered = exact;
        tampered.receipt_fingerprint = "different-stage".into();
        assert!(
            project_mcp_session_transport(
                &prepared,
                &exact_request,
                None,
                &tampered,
                awaken_run_executor_acp::acp_cli("claude").unwrap(),
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
        let request = request(awaken_runtime_contract::PlaintextBoundary::Worker);
        let receipt = legacy_receipt(&request);
        let adapter = awaken_run_executor_acp::acp_cli("codex").unwrap();
        // Without a live relay, fail closed: this process has no alternate
        // reference resolver and may not report a non-functional projection.
        assert!(project_mcp_session_transport(&p, &request, None, &receipt, adapter).is_err());
        // Anonymous access remains a direct secret-free route.
        let mut anonymous = request;
        anonymous.credential = None;
        anonymous.selected_plaintext_holder = None;
        let receipt = legacy_receipt(&anonymous);
        assert!(
            project_mcp_session_transport(&prepared(None), &anonymous, None, &receipt, adapter,)
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
        let mut request = request(awaken_runtime_contract::PlaintextBoundary::Worker);
        request.name = "playwright".into();
        request.target = awaken_session_contract::McpTarget::sandbox_stdio(
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
            awaken_run_executor_acp::acp_cli("codex").unwrap(),
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
        let request = request(awaken_runtime_contract::PlaintextBoundary::Worker);
        let receipt = legacy_receipt(&request);
        relay.set_route(&request.generation, &p);
        // Sandboxed + relay: the projected server dials the relay (loopback), holds NO
        // credential (the relay injects the real bearer host-side), never the raw secret.
        let s = project_mcp_session_transport(
            &p,
            &request,
            Some(&relay),
            &receipt,
            awaken_run_executor_acp::acp_cli("codex").unwrap(),
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
        // deadline; E2 the discovered executable reports a transport failure;
        // E3 upstream observes exactly one tools/call. Rule H1 C1+C2+C3 ->
        // E1+E2+E3. Constraint: production calls the same helper with the private
        // 300s policy; neither attachment desired state nor probe wiring carries
        // this override. FMECA: omitting the host builder override silently leaves
        // materialized calls on the shared 30s default.
        assert_eq!(
            MATERIALIZED_HTTP_MCP_TOOL_CALL_TIMEOUT,
            Duration::from_secs(300)
        );
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
            .invoke(awaken_runtime_contract::tool::ToolCall {
                call_id: "call-1".into(),
                tool_id: "mcp__docs__echo".into(),
                arguments: serde_json::json!({ "value": "slow" }),
            })
            .await
            .expect_err("H1/E2: injected tool deadline is wired to execution");
        assert!(
            matches!(
                &error,
                awaken_runtime_contract::tool::ToolError::Execution(_)
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
