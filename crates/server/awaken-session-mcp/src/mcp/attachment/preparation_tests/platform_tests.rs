// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-mcp/src/mcp/attachment.rs
// @formal reason: Installed PlatformRelay provisioner fault cuts; no external mediator or plaintext enforcement claim.
use super::*;
use awaken_agent_contract::{
    credential::CredentialAccess, credential::CredentialExecutionPolicy,
    credential::CredentialMaterialSource, credential::CredentialRef, credential::CredentialUsage,
    credential::ModelExposurePolicy, credential::PlaintextBoundary, credential::PlaintextHolder,
};
use awaken_credential_contract::{
    CredentialMaterialCapability, CredentialMaterialError, CredentialMaterialRequest,
    CredentialMaterialResolver, ResolvedCredentialMaterial,
};
use awaken_credential_materializer::{MediatedMcpRoute, PlatformMcpEgressRequest};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

#[path = "platform_tests/release_tests.rs"]
mod release_tests;

pub(super) struct PlatformInputs {
    pub(super) materializer: PinnedCredentialMaterializer,
    pub(super) provisioner: Arc<Provisioner>,
}

#[derive(Clone)]
pub(super) struct Provisioner {
    entered: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
    release_entered: Arc<tokio::sync::Semaphore>,
    quiesced: Arc<tokio::sync::Semaphore>,
    quiescence: Arc<tokio::sync::Semaphore>,
    acknowledgment: Arc<tokio::sync::Semaphore>,
    release_failures: Arc<AtomicUsize>,
    lose_provision_reply: Arc<AtomicBool>,
    observed: Arc<std::sync::Mutex<Vec<AllocationObservation>>>,
}

#[derive(Clone)]
struct AllocationObservation {
    realization: String,
    stage: String,
    issued: Arc<AtomicBool>,
    // Model states of the installed test mediator: issuable, admission closed,
    // then accepted calls quiescent. Host completion is asserted separately.
    phase: Arc<AtomicU8>,
}

struct Allocation {
    provider: Provisioner,
    observed: AllocationObservation,
    activity: tokio::sync::Mutex<()>,
}

impl Provisioner {
    fn new() -> Self {
        Self {
            entered: Arc::new(tokio::sync::Semaphore::new(0)),
            release: Arc::new(tokio::sync::Semaphore::new(0)),
            release_entered: Arc::new(tokio::sync::Semaphore::new(0)),
            quiesced: Arc::new(tokio::sync::Semaphore::new(0)),
            quiescence: Arc::new(tokio::sync::Semaphore::new(1)),
            acknowledgment: Arc::new(tokio::sync::Semaphore::new(1)),
            release_failures: Arc::new(AtomicUsize::new(0)),
            lose_provision_reply: Arc::new(AtomicBool::new(false)),
            observed: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

impl PlatformMcpEgressProvisioner for Provisioner {
    fn prepare(
        &self,
        request: PlatformMcpEgressRequest<'_>,
    ) -> Arc<dyn awaken_credential_materializer::PlatformMcpEgressAllocation> {
        assert_eq!(request.workspace_id, "workspace");
        assert_eq!(request.session_id, "session");
        assert_eq!(request.credential.id, "platform-credential");
        assert_eq!(request.credential.revision, 7);
        assert_eq!(request.runtime_incarnation, "runtime");
        assert_eq!(request.lease_epoch, 1);
        assert_eq!(request.attachment_id, "docs");
        assert_eq!(request.generation, 1);
        assert_eq!(request.upstream_url, "https://fixture.test/mcp");
        assert_eq!(request.holder, &holder());
        assert!(!request.realization_id.is_empty());
        assert!(!request.stage_idempotency_key.is_empty());
        let observed = AllocationObservation {
            realization: request.realization_id.to_owned(),
            stage: request.stage_idempotency_key.to_owned(),
            issued: Arc::new(AtomicBool::new(false)),
            phase: Arc::new(AtomicU8::new(0)),
        };
        self.observed.lock().unwrap().push(observed.clone());
        Arc::new(Allocation {
            provider: self.clone(),
            observed,
            activity: tokio::sync::Mutex::new(()),
        })
    }
}

#[async_trait::async_trait]
impl awaken_credential_materializer::PlatformMcpEgressAllocation for Allocation {
    async fn provision(&self) -> Result<MediatedMcpRoute, CredentialMaterialError> {
        let _activity = self.activity.lock().await;
        if self.observed.phase.load(Ordering::SeqCst) != 0 {
            return Err(CredentialMaterialError::Unavailable);
        }
        if !self.observed.issued.load(Ordering::SeqCst) {
            self.provider.entered.add_permits(1);
            self.provider.release.acquire().await.unwrap().forget();
            self.observed.issued.store(true, Ordering::SeqCst);
            if self
                .provider
                .lose_provision_reply
                .swap(false, Ordering::SeqCst)
            {
                return Err(CredentialMaterialError::Unavailable);
            }
        }
        Ok(MediatedMcpRoute {
            url: "https://mediator.test/opaque-route".into(),
            expires_at_unix_ms: u64::MAX,
        })
    }

    async fn revoke_and_wait(&self) -> Result<(), CredentialMaterialError> {
        let _activity = self.activity.lock().await;
        if self.observed.phase.load(Ordering::SeqCst) == 2 {
            return Ok(());
        }
        self.provider.release_entered.add_permits(1);
        if self
            .provider
            .release_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(CredentialMaterialError::Unavailable);
        }
        self.observed.phase.store(1, Ordering::SeqCst);
        self.provider.quiescence.acquire().await.unwrap().forget();
        self.observed.phase.store(2, Ordering::SeqCst);
        self.provider.quiesced.add_permits(1);
        self.provider
            .acknowledgment
            .acquire()
            .await
            .unwrap()
            .forget();
        Ok(())
    }
}

struct NoPlaintext;
#[async_trait::async_trait]
impl CredentialMaterialResolver for NoPlaintext {
    async fn validate_reference(
        &self,
        request: CredentialMaterialRequest<'_>,
    ) -> Result<(), CredentialMaterialError> {
        // This component fixture represents an available external metadata
        // authority. It checks the selected reference but cannot return secrets.
        assert_eq!(request.access.credential.id, "platform-credential");
        assert_eq!(request.access.credential.revision, 7);
        assert_eq!(request.selected_holder, &holder());
        request.binding.validate()
    }

    fn material_capabilities(&self) -> BTreeSet<CredentialMaterialCapability> {
        [CredentialMaterialCapability {
            holder: holder(),
            source: CredentialMaterialSource::ControlPlaneReference,
            envelope: false,
        }]
        .into()
    }
    async fn resolve_exact(
        &self,
        _: CredentialMaterialRequest<'_>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        panic!("PlatformRelay must never resolve plaintext into the Runtime Host")
    }
}

fn holder() -> PlaintextHolder {
    PlaintextHolder::new(PlaintextBoundary::Platform, "fixture.platform")
}

struct PlatformPreparation {
    request: StageMcpAttachment,
    gate: Arc<Provisioner>,
    task: tokio::task::JoinHandle<
        Result<awaken_session_configuration_contract::mcp::McpRealizationReceipt, RunError>,
    >,
}

fn begin(mcp: &McpSessions, slots: &Arc<Slots>) -> PlatformPreparation {
    let mut request = request("https://fixture.test/mcp");
    request.credential = Some(
        CredentialAccess::new(
            CredentialRef {
                id: "platform-credential".into(),
                revision: 7,
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            CredentialExecutionPolicy::exact(holder(), ModelExposurePolicy::VirtualOnly),
        )
        .with_target(awaken_agent_contract::credential::CredentialTarget::new(
            awaken_agent_contract::credential::CredentialPurpose::McpAuthorization,
            "https://fixture.test/mcp",
        )),
    );
    request.selected_plaintext_holder = Some(holder());
    let provisioner = Arc::new(Provisioner::new());
    let context = Arc::new(Context {
        slots: slots.clone(),
        realization_gate: None,
        platform: Some(PlatformInputs {
            materializer: PinnedCredentialMaterializer::external_only(
                Arc::new(NoPlaintext),
                awaken_time::system_clock(),
            ),
            provisioner: provisioner.clone(),
        }),
    });
    let sessions = mcp.clone();
    let slots = slots.clone();
    let input = request.clone();
    let task = tokio::spawn(async move {
        sessions
            .stage_assigned_attachment(&slots, context, credentials, input)
            .await
    });
    PlatformPreparation {
        request,
        gate: provisioner,
        task,
    }
}

#[tokio::test]
async fn platform_route_preparation_is_retained_after_waiter_cancel_and_drain_waits() {
    let slots = Slots::live();
    let mcp = McpSessions::new(None, awaken_time::system_clock());
    let PlatformPreparation {
        request,
        gate,
        mut task,
    } = begin(&mcp, &slots);
    tokio::select! {
        entered = gate.entered.acquire() => entered.unwrap().forget(),
        result = &mut task => panic!("preparation exited before the provisioner fault cut: {result:?}"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => panic!("provisioner fault cut was never reached"),
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let original = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert_eq!(original.state, McpProjectionState::Staging);
    assert!(
        original.platform_egress.is_some(),
        "release capability precedes provision IO"
    );
    let drain = mcp.drain_selected_projection(slots.as_ref(), &original);
    tokio::pin!(drain);
    assert!(futures_util::poll!(drain.as_mut()).is_pending());
    gate.release.add_permits(1);
    drain.await.unwrap();
    let removed = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert!(removed.effects_cleared());
}

#[tokio::test]
async fn revoked_root_during_platform_provisioning_cannot_publish_the_returned_route() {
    let slots = Slots::live();
    let mcp = McpSessions::new(None, awaken_time::system_clock());
    let PlatformPreparation {
        request,
        gate,
        mut task,
    } = begin(&mcp, &slots);
    tokio::select! {
        entered = gate.entered.acquire() => entered.unwrap().forget(),
        result = &mut task => panic!("preparation exited before the provisioner fault cut: {result:?}"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => panic!("provisioner fault cut was never reached"),
    }
    slots.0.lock().unwrap().root = Some(false);
    gate.release.add_permits(1);
    assert_eq!(task.await.unwrap().unwrap_err().code, "mcp_stale_ownership");
    let retained = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert_eq!(retained.state, McpProjectionState::Draining);
    assert!(retained.platform_egress.as_ref().unwrap().route().is_some());
    assert!(
        retained.server.is_none(),
        "cleanup metadata is not published material"
    );
    assert!(retained.native_wiring.is_none());
    mcp.drain_selected_projection(slots.as_ref(), &retained)
        .await
        .unwrap();
    let removed = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert!(removed.effects_cleared());
}
