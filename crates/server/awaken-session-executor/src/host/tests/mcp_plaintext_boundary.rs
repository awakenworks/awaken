//! T15-C03: a Platform-held MCP credential crosses the Runtime Host only as a
//! reference. Receipts, projections and refusals never carry its plaintext,
//! and the Host never stands in for the egress mediator.

use super::*;
use awaken_session_execution_contract::McpAttachmentRealizer as _;

const SENTINEL: &str = "SENTINEL-platform-plaintext-7f3a";
const UPSTREAM: &str = "https://upstream.invalid/mcp";

/// The deployment's mediator stand-in. It records each exact reference it was
/// asked to bind and may be scripted to refuse; it never sees material.
struct RecordingMediator {
    url: String,
    fail: std::sync::atomic::AtomicBool,
    provisioned: std::sync::Mutex<Vec<awaken_agent_contract::credential::CredentialRef>>,
    gate: Arc<ProvisionGate>,
}

struct ProvisionGate {
    pause: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Semaphore,
    revoked: std::sync::atomic::AtomicUsize,
}

impl awaken_credential_materializer::PlatformMcpEgressProvisioner for RecordingMediator {
    fn prepare(
        &self,
        request: awaken_credential_materializer::PlatformMcpEgressRequest<'_>,
    ) -> Arc<dyn awaken_credential_materializer::PlatformMcpEgressAllocation> {
        self.provisioned
            .lock()
            .unwrap()
            .push(request.credential.clone());
        Arc::new(RecordingAllocation {
            url: self.url.clone(),
            fail: self.fail.load(std::sync::atomic::Ordering::SeqCst),
            gate: self.gate.clone(),
        })
    }
}

/// The one allocation a preparation captured; refuses when the mediator was
/// scripted to refuse at that preparation.
struct RecordingAllocation {
    url: String,
    fail: bool,
    gate: Arc<ProvisionGate>,
}

#[async_trait::async_trait]
impl awaken_credential_materializer::PlatformMcpEgressAllocation for RecordingAllocation {
    async fn provision(
        &self,
    ) -> Result<
        awaken_credential_materializer::MediatedMcpRoute,
        awaken_credential_contract::CredentialMaterialError,
    > {
        if self.gate.pause.load(std::sync::atomic::Ordering::SeqCst) {
            self.gate.entered.notify_one();
            self.gate.resume.acquire().await.unwrap().forget();
        }
        if self.fail {
            return Err(awaken_credential_contract::CredentialMaterialError::Unavailable);
        }
        Ok(awaken_credential_materializer::MediatedMcpRoute {
            url: self.url.clone(),
            expires_at_unix_ms: u64::MAX,
        })
    }

    async fn revoke_and_wait(
        &self,
    ) -> Result<(), awaken_credential_contract::CredentialMaterialError> {
        self.gate
            .revoked
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// Every named surface whose text contains the plaintext sentinel.
fn plaintext_leaks(surfaces: &[(&'static str, String)]) -> Vec<&'static str> {
    surfaces
        .iter()
        .filter(|(_, text)| text.contains(SENTINEL))
        .map(|(name, _)| *name)
        .collect()
}

struct PlatformFixture {
    host: Arc<ExecutorComponents>,
    mediator: Arc<RecordingMediator>,
    credentials: Arc<awaken_credential_vault::repo::InMemoryCredentialRepo>,
    credential: awaken_credential_vault::CredentialSource,
}

/// One stored Vault credential whose secret is the sentinel and whose
/// descriptor authorizes exactly the upstream MCP audience, so a Host that
/// resolved Platform-held material would obtain the sentinel.
pub(super) async fn platform_credential() -> (
    Arc<awaken_credential_vault::repo::InMemoryCredentialRepo>,
    Arc<awaken_credential_vault::InMemorySecretStore>,
    awaken_credential_vault::CredentialSource,
) {
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let mut credential = awaken_credential_vault::repo::enter_credential(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: "workspace-a".into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(awaken_agent_contract::RedactedString::from(
                SENTINEL.to_string(),
            )),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
        awaken_time::system_clock().as_ref(),
    )
    .await
    .unwrap();
    let audience = awaken_agent_contract::McpTarget::identity(UPSTREAM)
        .unwrap()
        .canonical_url();
    credential.descriptor = Some(awaken_credential_contract::CredentialDescriptor::new(
        audience.clone(),
        awaken_credential_contract::CredentialMaterialDescriptor::secret(
            awaken_credential_contract::OPAQUE_SECRET_MATERIAL_TYPE,
        ),
        [awaken_credential_contract::CredentialTargetContract::new(
            awaken_agent_contract::credential::CredentialTarget::new(
                awaken_agent_contract::credential::CredentialPurpose::McpAuthorization,
                audience,
            ),
            awaken_agent_contract::credential::CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
        )],
    ));
    awaken_credential_vault::repo::CredentialRepo::put(credentials.as_ref(), credential.clone())
        .await
        .unwrap();
    (credentials, secrets, credential)
}

async fn platform_fixture() -> PlatformFixture {
    let (mediated_url, _seen) = crate::test_mcp::start(None).await;
    platform_fixture_at(mediated_url).await
}

async fn platform_fixture_at(mediated_url: String) -> PlatformFixture {
    let (credentials, secrets, credential) = platform_credential().await;
    let mediator = Arc::new(RecordingMediator {
        url: mediated_url,
        fail: std::sync::atomic::AtomicBool::new(false),
        provisioned: std::sync::Mutex::new(Vec::new()),
        gate: Arc::new(ProvisionGate {
            pause: std::sync::atomic::AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            resume: tokio::sync::Semaphore::new(0),
            revoked: std::sync::atomic::AtomicUsize::new(0),
        }),
    });
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_credentials(credentials.clone(), secrets)
        .with_session_execution(
            crate::SessionExecutionDependencies::default()
                .with_platform_mcp_egress_provisioner(mediator.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    PlatformFixture {
        host,
        mediator,
        credentials,
        credential,
    }
}

pub(super) fn credential_ref(
    credential: &awaken_credential_vault::CredentialSource,
) -> awaken_agent_contract::credential::CredentialRef {
    awaken_agent_contract::credential::CredentialRef {
        id: credential.id.0.clone(),
        revision: u64::try_from(credential.version).expect("nonnegative revision"),
    }
}

/// Install the Session lease the assigned staging path requires and return
/// one Platform-held request for `session`.
fn platform_request(
    host: &ExecutorComponents,
    session: &str,
    credential: awaken_agent_contract::credential::CredentialRef,
) -> crate::session_mcp::StageMcpAttachment {
    use awaken_agent_contract::{
        credential::CredentialAccess, credential::CredentialExecutionPolicy,
        credential::CredentialMaterialSource, credential::CredentialUsage,
        credential::ModelExposurePolicy, credential::PlaintextBoundary,
        credential::PlaintextHolder,
    };
    host.run_assembly
        .install_session_realization_lease(
            session,
            crate::session_resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "runtime-1".into(),
                epoch: 1,
                expires_at_unix_ms: u64::MAX,
            },
        )
        .unwrap();
    let holder = PlaintextHolder::new(
        PlaintextBoundary::Platform,
        awaken_credential_contract::PLATFORM_EGRESS_GATEWAY_TRUST_DOMAIN,
    );
    let target = awaken_agent_contract::McpTarget::parse_http(UPSTREAM).unwrap();
    let generation = crate::session_mcp::McpGenerationRef::from_persisted(
        session.into(),
        crate::session_mcp::McpGeneration::of_bare_target(
            crate::session_mcp::McpAttachmentId("mcp-docs".into()),
            1,
            &target,
        ),
        "runtime-1".into(),
        1,
        u64::MAX - 1,
    )
    .expect("MCP generation fence epoch is nonzero");
    crate::session_mcp::StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation,
        realization_id: format!("realize-{session}"),
        stage_idempotency_key: format!("stage-{session}"),
        name: "docs".into(),
        target,
        prompts_as_skills: false,
        credential: Some(
            CredentialAccess::new(
                credential,
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::VirtualOnly),
            )
            .with_target(awaken_agent_contract::credential::CredentialTarget::new(
                awaken_agent_contract::credential::CredentialPurpose::McpAuthorization,
                awaken_agent_contract::McpTarget::identity(UPSTREAM)
                    .unwrap()
                    .canonical_url(),
            )),
        ),
        selected_plaintext_holder: Some(holder),
    }
}

/// | Rule | mediator | Effect |
/// |---|---|---|
/// | P1 | accepts | receipt, request and projection carry the exact reference only |
/// | P2 | refuses | stable `mcp_gateway_route_unavailable`; the refusal carries no plaintext |
/// | NC | a resolving Host's projection | the same leak check names the bearer |
#[tokio::test]
async fn platform_mcp_receipt_projection_and_refusal_carry_no_plaintext() {
    let fixture = platform_fixture().await;
    let reference = credential_ref(&fixture.credential);
    let request = platform_request(&fixture.host, "mcp-plaintext", reference.clone());
    let generation = request.generation.clone();
    let receipt = fixture
        .host
        .run_assembly
        .stage_mcp_attachment(request.clone())
        .await
        .expect("P1 mediated staging");
    assert_eq!(
        receipt.actual_realization_kind,
        Some(awaken_credential_contract::CredentialRealizationKind::PlatformRelay),
        "P1"
    );
    assert_eq!(
        *fixture.mediator.provisioned.lock().unwrap(),
        std::slice::from_ref(&reference),
        "P1 the mediator binds exactly the stored reference, once"
    );
    let projection = fixture
        .host
        .run_assembly
        .mcp_projection(&generation)
        .expect("P1 projection");
    let server = projection.server.as_ref().expect("P1 transport");
    assert!(server.bearer().is_none(), "P1 no Host-held bearer");
    let surfaces = [
        ("receipt", serde_json::to_string(&receipt).unwrap()),
        ("request", serde_json::to_string(&request).unwrap()),
        (
            "projected receipt",
            serde_json::to_string(&projection.receipt).unwrap(),
        ),
        (
            "projected route",
            server.http_url().unwrap_or_default().to_owned(),
        ),
    ];
    assert_eq!(plaintext_leaks(&surfaces), Vec::<&str>::new(), "P1");

    fixture
        .mediator
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let refused = fixture
        .host
        .run_assembly
        .stage_mcp_attachment(platform_request(
            &fixture.host,
            "mcp-plaintext-refused",
            reference,
        ))
        .await
        .expect_err("P2 a refused mediator route is a refusal");
    assert_eq!(refused.code, "mcp_gateway_route_unavailable", "P2");
    assert_eq!(
        refused.kind,
        awaken_session_types::RunErrorKind::Unavailable,
        "P2 retryable"
    );
    assert_eq!(
        plaintext_leaks(&[("refusal", format!("{refused:?} {}", refused.message))]),
        Vec::<&str>::new(),
        "P2"
    );

    // NC: the projection a resolving Host would install. The same check must
    // name it, so P1 cannot pass vacuously.
    let resolved = awaken_session_mcp::McpTransportMaterial {
        name: "docs".into(),
        prompts_as_skills: false,
        transport: awaken_session_mcp::McpTransportMaterialKind::Http {
            url: UPSTREAM.into(),
            bearer: Some(awaken_agent_contract::RedactedString::new(SENTINEL)),
            refresh: None,
        },
    };
    let mutant_bearer = resolved
        .bearer()
        .map(|bearer| bearer.expose_secret().to_owned())
        .unwrap_or_default();
    assert_eq!(
        plaintext_leaks(&[("resolved bearer", mutant_bearer)]),
        ["resolved bearer"],
        "NC a resolving Host is detected"
    );
}

#[tokio::test]
async fn disabled_platform_credential_is_refused_before_a_mediated_route() {
    // D1 active credential becomes Disabled before staging => reference-only
    // refusal, zero mediator preparations and no usable projection. Source
    // metadata is checked by the existing Vault authority, never by resolving
    // the Platform-held secret into this caller.
    let fixture = platform_fixture().await;
    let mut disabled = fixture.credential.clone();
    disabled.status = awaken_credential_vault::CredentialStatus::Disabled;
    awaken_credential_vault::repo::CredentialRepo::put(fixture.credentials.as_ref(), disabled)
        .await
        .unwrap();
    let request = platform_request(
        &fixture.host,
        "mcp-disabled",
        credential_ref(&fixture.credential),
    );
    let generation = request.generation.clone();
    let staged = fixture
        .host
        .run_assembly
        .stage_mcp_attachment(request)
        .await;
    assert!(
        staged.is_err(),
        "a disabled credential must not obtain a mediated route"
    );
    assert!(
        fixture.mediator.provisioned.lock().unwrap().is_empty(),
        "no mediated route is minted for a disabled credential"
    );
    assert!(
        fixture
            .host
            .run_assembly
            .mcp_projection(&generation)
            .is_none_or(|projection| projection.server.is_none()),
        "no usable projection"
    );
}

#[tokio::test]
async fn platform_credential_revoked_during_provision_is_drained_without_publication() {
    // D2: Active -> provision barrier -> Disabled -> refusal and owned drain.
    revoked_platform_preparation(None).await;
}

#[tokio::test]
async fn platform_credential_revoked_during_initialize_is_drained_without_publication() {
    // D3: Active at provision return -> initialize barrier -> Disabled -> no
    // receipt/wiring/server publication; drain the original minted allocation.
    let gate = Arc::new(crate::test_mcp::InitializeGate::new());
    revoked_platform_preparation(Some(gate)).await;
}

async fn revoked_platform_preparation(initialize: Option<Arc<crate::test_mcp::InitializeGate>>) {
    // | Rule | source before IO | source after IO | Effect                    |
    // | D2   | Active/provision | Disabled        | refuse + retain/drain owner|
    // | D3   | Active/initialize| Disabled        | refuse + retain/drain owner|
    // D1 above covers zero-effect refusal. D2/D3 observe an already
    // created allocation: it must not claim zero effects or discard cleanup.
    // Each barrier commits revocation before its original IO returns.
    let fixture = if let Some(gate) = &initialize {
        let (url, _seen) = crate::test_mcp::start_with_initialize_gate(gate.clone()).await;
        platform_fixture_at(url).await
    } else {
        platform_fixture().await
    };
    let request = platform_request(
        &fixture.host,
        "mcp-revoked-in-flight",
        credential_ref(&fixture.credential),
    );
    let generation = request.generation.clone();
    fixture
        .mediator
        .gate
        .pause
        .store(initialize.is_none(), std::sync::atomic::Ordering::SeqCst);
    let (staged, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            fixture.host.run_assembly.stage_mcp_attachment(request),
            async {
                if let Some(gate) = &initialize {
                    gate.entered().await;
                } else {
                    fixture.mediator.gate.entered.notified().await;
                }
                let mut disabled = fixture.credential.clone();
                disabled.status = awaken_credential_vault::CredentialStatus::Disabled;
                awaken_credential_vault::repo::CredentialRepo::put(
                    fixture.credentials.as_ref(),
                    disabled,
                )
                .await
                .unwrap();
                if let Some(gate) = &initialize {
                    gate.release();
                } else {
                    fixture.mediator.gate.resume.add_permits(1);
                }
            }
        )
    })
    .await
    .expect("D2 provision and revocation settle");
    assert!(
        staged.is_err(),
        "D2 no receipt from stale credential metadata"
    );
    assert_eq!(
        fixture.mediator.provisioned.lock().unwrap().len(),
        1,
        "D2 real preparation happened"
    );
    let projection = fixture
        .host
        .run_assembly
        .mcp_projection(&generation)
        .expect("D2 cleanup responsibility retained");
    assert!(
        projection.server.is_none() && projection.native_wiring.is_none(),
        "D2/D3 no server or wiring published"
    );
    fixture
        .host
        .run_assembly
        .drain_mcp_generation(generation.clone())
        .await
        .unwrap();
    assert!(
        fixture
            .mediator
            .gate
            .revoked
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0,
        "D2 original allocation acknowledged revocation"
    );
    assert!(
        fixture
            .host
            .run_assembly
            .mcp_projection(&generation)
            .is_none_or(|p| p.server.is_none() && p.native_wiring.is_none()),
        "D2 remains unavailable"
    );
}
