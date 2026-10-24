//! T16-C01: unified MCP preparation window.
//!
//! Production entrypoint: `McpAttachmentRealizer::stage_mcp_attachment` on the
//! Host's `RunAssembly` (assigned admission: the Session root must be
//! installed and live). Collaborators only are fakes: the gated MCP HTTP
//! fixture (`test_mcp::start_with_initialize_gate`) and a gated Platform
//! egress provisioner.
//!
//! Decision table (each event is fired only after the barrier confirms the
//! external preparation step was entered):
//! | branch                  | barrier          | event                         | expected                               |
//! | Native HTTP, no cred    | initialize reply | close admission               | Staging owned at gate; Err quiescing;  |
//! |                         |                  |                               | no Staged/Active, no wiring            |
//! |                         |                  | reinstall root (new alloc)    | Err owner changed; new alloc empty     |
//! |                         |                  | lease expiry                  | Err stale ownership                    |
//! | PlatformRelay           | provision        | same three events             | Staging owned at gate; one provision;  |
//! |                         |                  |                               | Err; nothing published                 |
//! Negative control (executed): an in-test mutant that performs the external
//! I/O before beginning the staging reservation is detected by the same
//! at-gate staging oracle. (The "only check the root at final insert" mutant
//! is covered against the replacement allocation by session-mcp's
//! `a_late_native_connection_never_publishes_into_the_same_named_replacement_allocation`.)
//! Known gap (ignored reproducer): a PlatformRelay route minted during the
//! window has no cleanup owner once publication is refused.
//! Native Worker-held and ACP WorkerRelay rows live in `credential_branches`.
//! Not covered here: the sandbox-stdio branch (needs a gated SessionEnvironment).

use super::*;

mod credential_branches;
use awaken_session_execution_contract::McpAttachmentRealizer as _;
use awaken_session_mcp::{McpGenerationProjection, McpProjectionState};

const INCARNATION: &str = "runtime-1";

fn lease() -> crate::session_resource::SessionRealizationLease {
    crate::session_resource::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: INCARNATION.into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Event {
    CloseAdmission,
    ReinstallRoot,
    LeaseExpiry,
}

const EVENTS: [Event; 3] = [
    Event::CloseAdmission,
    Event::ReinstallRoot,
    Event::LeaseExpiry,
];

impl Event {
    fn expected_code(self) -> &'static str {
        match self {
            Self::CloseAdmission => "session_environment_quiescing",
            Self::ReinstallRoot => "mcp_generation_owner_changed",
            Self::LeaseExpiry => "mcp_stale_ownership",
        }
    }
}

fn fire(host: &ExecutorComponents, session: &str, event: Event) {
    match event {
        Event::CloseAdmission => {
            let sandbox_generation = awaken_session_state_contract::SandboxGeneration::new(
                &awaken_session_types::SessionId::from(session),
                1,
                100,
                "environment",
                "base",
            );
            let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
                "workspace-a",
                &awaken_session_types::SessionId::from(session),
                "suspend",
                &sandbox_generation,
                3,
                None,
                None,
            );
            host.run_assembly
                .installed_sessions
                .realizations()
                .close_mcp_realization_admission(
                    session,
                    awaken_session_mcp::McpQuiescenceAdmissionFence::new(
                        &operation,
                        "source-effect",
                        "source-binding",
                        &sandbox_generation,
                    ),
                )
                .unwrap();
        }
        Event::ReinstallRoot => {
            host.run_assembly.installed_sessions.remove(session);
            host.run_assembly
                .install_session_realization_lease(session, lease())
                .unwrap();
        }
        Event::LeaseExpiry => {
            host.run_assembly
                .installed_sessions
                .realizations()
                .update_realization(session, |r| {
                    let mut expired = lease();
                    expired.expires_at_unix_ms = 1;
                    r.execution_mut().authority =
                        crate::realization_state::SessionRealizationAuthority::Live(
                            awaken_session_realization::test_support::test_expired_observed_lease(
                                expired,
                            ),
                        );
                })
                .expect("installed Sessions");
        }
    }
}

/// Oracle 0: at the external-I/O barrier, the exact generation already has
/// a Staging owner holding the preparation activity and nothing published.
fn check_staged_before_io(projection: Option<&McpGenerationProjection>) -> Result<(), String> {
    let Some(projection) = projection else {
        return Err("no staging reservation exists while external I/O is in flight".into());
    };
    if projection.state != McpProjectionState::Staging {
        return Err(format!(
            "at the barrier the projection is {:?}",
            projection.state
        ));
    }
    if projection.staging.is_none() {
        return Err("the Staging projection does not own the preparation activity".into());
    }
    if projection.server.is_some() || projection.native_wiring.is_some() {
        return Err("material was published before the preparation finished".into());
    }
    Ok(())
}

/// Oracles 1-2: after the window closed, the old task published nothing:
/// no Staged/Active projection, no transport material or wiring, and no
/// visible projection in the (possibly new) allocation.
fn assert_nothing_published(
    host: &ExecutorComponents,
    session: &str,
    generation: &crate::session_mcp::McpGenerationRef,
    row: &str,
) {
    if let Some(projection) = host.run_assembly.mcp_projection(generation) {
        assert!(
            matches!(
                projection.state,
                McpProjectionState::Removed | McpProjectionState::Draining
            ),
            "{row}: late task left {:?}",
            projection.state
        );
        assert!(projection.server.is_none(), "{row}: no transport material");
        assert!(
            projection.native_wiring.is_none(),
            "{row}: no native wiring"
        );
    }
    if let Ok(active) = host.run_assembly.active_mcp_projections(session) {
        assert!(
            active.is_empty(),
            "{row}: nothing visible in the allocation"
        );
    }
}

fn native_request(session: &str, url: &str) -> crate::session_mcp::StageMcpAttachment {
    crate::session_mcp::StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: crate::test_mcp::mcp_fence(session, "docs", 1, INCARNATION, 1, u64::MAX),
        realization_id: "realization-docs".into(),
        stage_idempotency_key: "stage-docs".into(),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http(url).unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    }
}

fn initialize_count(seen: &crate::test_mcp::SeenMcpRequests) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(method, _)| method == "initialize")
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t16_c01_native_http_preparation_is_staged_before_io_and_fenced_after_the_wait() {
    for event in EVENTS {
        let row = format!("native-http/{event:?}");
        let session = format!("mcp-window-native-{event:?}").to_lowercase();
        let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
            .build()
            .expect("validated Host dependencies");
        host.run_assembly
            .install_session_realization_lease(&session, lease())
            .unwrap();
        let gate = Arc::new(crate::test_mcp::InitializeGate::new());
        let (url, seen) = crate::test_mcp::start_with_initialize_gate(gate.clone()).await;
        let request = native_request(&session, &url);
        let generation = request.generation.clone();
        let assembly = host.run_assembly.clone();
        let pending = tokio::spawn(async move { assembly.stage_mcp_attachment(request).await });
        gate.entered().await;
        check_staged_before_io(host.run_assembly.mcp_projection(&generation).as_ref())
            .unwrap_or_else(|violation| panic!("{row}: {violation}"));
        assert_eq!(
            initialize_count(&seen),
            1,
            "{row}: the one external initialize"
        );

        fire(&host, &session, event);
        gate.release();
        let error = pending
            .await
            .unwrap()
            .expect_err("a preparation that outlived its window must not stage");
        assert_eq!(error.code, event.expected_code(), "{row}: {error:?}");
        assert_nothing_published(&host, &session, &generation, &row);
        assert_eq!(
            initialize_count(&seen),
            1,
            "{row}: no re-initialize after the wait"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t16_c01_io_before_staging_reservation_is_detected() {
    // Negative control, executed: an in-test stager that performs the
    // external initialize before beginning the staging reservation. At its
    // I/O barrier the same oracle must report the missing reservation.
    let session = "mcp-window-mutant";
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    host.run_assembly
        .install_session_realization_lease(session, lease())
        .unwrap();
    let gate = Arc::new(crate::test_mcp::InitializeGate::new());
    let (url, _seen) = crate::test_mcp::start_with_initialize_gate(gate.clone()).await;
    let request = native_request(session, &url);
    let generation = request.generation.clone();
    let assembly = host.run_assembly.clone();
    let mutant = tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .post(&url)
            .header("accept", "application/json, text/event-stream")
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                           "clientInfo": {"name": "mutant", "version": "0"}},
            }))
            .send()
            .await;
        assembly.stage_mcp_attachment(request).await
    });
    gate.entered().await;
    let violation = check_staged_before_io(host.run_assembly.mcp_projection(&generation).as_ref())
        .expect_err("NC: I/O before the staging reservation must be detected");
    assert!(
        violation.contains("no staging reservation"),
        "NC: {violation}"
    );
    gate.release();
    mutant.abort();
    let _ = mutant.await;
}

/// Platform egress provisioner gated inside `provision`.
struct GatedMediator {
    url: String,
    calls: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl awaken_credential_materializer::PlatformMcpEgressProvisioner for GatedMediator {
    async fn provision(
        &self,
        _request: awaken_credential_materializer::PlatformMcpEgressRequest<'_>,
    ) -> Result<
        awaken_credential_materializer::MediatedMcpRoute,
        awaken_credential_contract::CredentialMaterialError,
    > {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entered.notify_one();
        self.release.acquire().await.expect("gate open").forget();
        Ok(awaken_credential_materializer::MediatedMcpRoute {
            url: self.url.clone(),
            expires_at_unix_ms: u64::MAX,
        })
    }
}

fn platform_request(session: &str) -> crate::session_mcp::StageMcpAttachment {
    use awaken_credential_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };
    let holder = PlaintextHolder::new(
        PlaintextBoundary::Platform,
        awaken_credential_contract::PLATFORM_EGRESS_GATEWAY_TRUST_DOMAIN,
    );
    let upstream = "https://upstream.invalid/mcp";
    let audience = awaken_agent_contract::McpTarget::identity(upstream)
        .unwrap()
        .canonical_url();
    let generation = crate::session_mcp::McpGenerationRef::from_persisted(
        session.into(),
        crate::session_mcp::McpGeneration::of_bare_target(
            crate::session_mcp::McpAttachmentId("mcp-docs".into()),
            1,
            &awaken_agent_contract::McpTarget::parse_http(upstream).unwrap(),
        ),
        INCARNATION.into(),
        1,
        u64::MAX - 1,
    )
    .expect("MCP generation fence epoch is nonzero");
    crate::session_mcp::StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation,
        realization_id: "realize-platform".into(),
        stage_idempotency_key: "stage-platform".into(),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http(upstream).unwrap(),
        prompts_as_skills: false,
        credential: Some(
            CredentialAccess::new(
                CredentialRef {
                    id: "credential-never-stored".into(),
                    revision: 7,
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::VirtualOnly),
            )
            .with_target(awaken_agent_contract::credential::CredentialTarget::new(
                awaken_credential_contract::CredentialPurpose::McpAuthorization,
                audience,
            )),
        ),
        selected_plaintext_holder: Some(holder),
    }
}

struct PlatformRow {
    host: Arc<ExecutorComponents>,
    mediator: Arc<GatedMediator>,
    mediated_url: String,
    generation: crate::session_mcp::McpGenerationRef,
    error_code: String,
}

async fn platform_row(event: Event) -> PlatformRow {
    let row = format!("platform/{event:?}");
    let session = format!("mcp-window-platform-{event:?}").to_lowercase();
    let (mediated_url, _seen) = crate::test_mcp::start(None).await;
    let mediator = Arc::new(GatedMediator {
        url: mediated_url.clone(),
        calls: std::sync::atomic::AtomicUsize::new(0),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_credentials(
            Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
            Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        )
        .with_session_execution(
            crate::SessionExecutionDependencies::default()
                .with_platform_mcp_egress_provisioner(mediator.clone()),
        ))
    .build()
    .unwrap();
    host.run_assembly
        .install_session_realization_lease(&session, lease())
        .unwrap();
    let request = platform_request(&session);
    let generation = request.generation.clone();
    let assembly = host.run_assembly.clone();
    let pending = tokio::spawn(async move { assembly.stage_mcp_attachment(request).await });
    mediator.entered.notified().await;
    check_staged_before_io(host.run_assembly.mcp_projection(&generation).as_ref())
        .unwrap_or_else(|violation| panic!("{row}: {violation}"));
    fire(&host, &session, event);
    mediator.release.add_permits(1);
    let error = pending
        .await
        .unwrap()
        .expect_err("a provision that outlived its window must not stage");
    assert_eq!(
        mediator.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "{row}: exactly one external provision"
    );
    assert_eq!(error.code, event.expected_code(), "{row}: {error:?}");
    assert_nothing_published(&host, &session, &generation, &row);
    PlatformRow {
        host,
        mediator,
        mediated_url,
        generation,
        error_code: error.code.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t16_c01_platform_relay_provision_is_staged_before_io_and_never_published_late() {
    for event in EVENTS {
        let row = platform_row(event).await;
        assert_eq!(row.error_code, event.expected_code());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "product gap: a PlatformRelay route minted during the preparation window has no cleanup owner after publication is refused (port has no revoke and the route expiry is discarded)"]
async fn t16_c01_platform_relay_route_minted_in_the_window_keeps_a_cleanup_owner() {
    for event in EVENTS {
        let row = platform_row(event).await;
        // The external route exists (one provision). Until it is revoked or
        // expires, a retained owner must still name it for cleanup instead of
        // a local `Removed` that forgets it.
        let retained = row
            .host
            .run_assembly
            .mcp_projection(&row.generation)
            .unwrap_or_else(|| panic!("{event:?}: the minted route has no retained owner"));
        assert_ne!(
            retained.state,
            McpProjectionState::Removed,
            "{event:?}: Removed forgets the minted mediator route"
        );
        assert_eq!(
            retained
                .server
                .as_ref()
                .and_then(|server| server.http_url()),
            Some(row.mediated_url.as_str()),
            "{event:?}: the cleanup owner names the exact minted route"
        );
        assert_eq!(
            row.mediator.calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
}
