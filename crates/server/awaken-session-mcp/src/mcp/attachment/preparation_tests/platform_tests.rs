// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-mcp/src/mcp/attachment.rs
// @formal reason: Installed PlatformRelay provisioner fault cuts; no external mediator or plaintext enforcement claim.
use super::*;
use awaken_credential_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialCapability,
    CredentialMaterialError, CredentialMaterialRequest, CredentialMaterialResolver,
    CredentialMaterialSource, CredentialRef, CredentialUsage, ModelExposurePolicy,
    PlaintextBoundary, PlaintextHolder, ResolvedCredentialMaterial,
};
use awaken_credential_materializer::{MediatedMcpRoute, PlatformMcpEgressRequest};
use std::collections::BTreeSet;

pub(super) struct PlatformInputs {
    pub(super) materializer: PinnedCredentialMaterializer,
    pub(super) provisioner: Arc<Provisioner>,
}

pub(super) struct Provisioner {
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl PlatformMcpEgressProvisioner for Provisioner {
    async fn provision(
        &self,
        request: PlatformMcpEgressRequest<'_>,
    ) -> Result<MediatedMcpRoute, CredentialMaterialError> {
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
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        Ok(MediatedMcpRoute {
            url: "https://mediator.test/opaque-route".into(),
            expires_at_unix_ms: u64::MAX,
        })
    }
}

struct NoPlaintext;
#[async_trait::async_trait]
impl CredentialMaterialResolver for NoPlaintext {
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
        .with_target(awaken_credential_contract::CredentialTarget::new(
            awaken_credential_contract::CredentialPurpose::McpAuthorization,
            "https://fixture.test/mcp",
        )),
    );
    request.selected_plaintext_holder = Some(holder());
    let provisioner = Arc::new(Provisioner {
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(0),
    });
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
    let drain = mcp.drain_selected_projection(slots.as_ref(), &original);
    tokio::pin!(drain);
    assert!(futures_util::poll!(drain.as_mut()).is_pending());
    gate.release.add_permits(1);
    drain.await.unwrap();
    let removed = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert_eq!(removed.state, McpProjectionState::Removed);
    assert!(removed.server.is_none());
    assert!(removed.native_wiring.is_none());
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
    let removed = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert_eq!(removed.state, McpProjectionState::Removed);
    assert!(removed.server.is_none());
    assert!(removed.native_wiring.is_none());
}
