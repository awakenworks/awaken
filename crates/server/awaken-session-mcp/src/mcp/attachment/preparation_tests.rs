// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-mcp/src/mcp/attachment.rs
// @formal reason: Public staging/drain fault cuts with a real local MCP HTTP handshake and canonical slot state observations.
use super::*;
use crate::mcp::{McpError, McpGenerationAuthority};
use crate::mcp_material::{McpProjectionState, SessionMcpState};
use awaken_session_configuration_contract::mcp::{McpGenerationRef, StageMcpAttachment};
use std::sync::Mutex;

struct Slot {
    state: SessionMcpState,
    root: Option<bool>,
    runtime: Option<()>,
}

struct Slots(Mutex<Slot>);

impl Slots {
    fn live() -> Arc<Self> {
        Arc::new(Self(Mutex::new(Slot {
            state: SessionMcpState::default(),
            root: Some(true),
            runtime: None,
        })))
    }
}

impl SessionMcpStates for Slots {
    type Runtime = ();
    fn read_mcp<R>(
        &self,
        _: &str,
        f: impl FnOnce(&SessionMcpState) -> R,
    ) -> Result<Option<R>, McpError> {
        Ok(Some(f(&self.0.lock().unwrap().state)))
    }
    fn update_mcp<R>(
        &self,
        _: &str,
        f: impl FnOnce(&mut SessionMcpState) -> R,
    ) -> Result<R, McpError> {
        Ok(f(&mut self.0.lock().unwrap().state))
    }
    fn modify_mcp<R>(
        &self,
        _: &str,
        f: impl FnOnce(&mut SessionMcpState) -> R,
    ) -> Result<Option<R>, McpError> {
        Ok(Some(f(&mut self.0.lock().unwrap().state)))
    }
    fn modify_mcp_with_root<R>(
        &self,
        _: &McpGenerationRef,
        f: impl FnOnce(&mut SessionMcpState, Option<bool>) -> R,
    ) -> Result<Option<R>, McpError> {
        let mut slot = self.0.lock().unwrap();
        let root = slot.root;
        Ok(Some(f(&mut slot.state, root)))
    }
    fn modify_mcp_and_runtime<R>(
        &self,
        _: &str,
        f: impl FnOnce(&mut SessionMcpState, &mut Option<()>) -> R,
    ) -> Result<Option<R>, McpError> {
        let mut slot = self.0.lock().unwrap();
        let Slot { state, runtime, .. } = &mut *slot;
        Ok(Some(f(state, runtime)))
    }
    fn cancel_runtime(_: &()) {}
    fn runtime_has_active_run(_: &()) -> bool {
        false
    }
}

impl McpGenerationAuthority for Slots {
    fn mcp_generation_root_authority(
        &self,
        _: &McpGenerationRef,
    ) -> Result<Option<bool>, RunError> {
        Ok(self.0.lock().unwrap().root)
    }
}

struct Context {
    slots: Arc<Slots>,
    platform: Option<platform::PlatformInputs>,
    realization_gate: Option<Arc<crate::test_mcp::InitializeGate>>,
}

#[path = "preparation_tests/platform_tests.rs"]
mod platform;

#[async_trait::async_trait]
impl McpStagingSession for Context {
    fn mcp_generation_is_authorized_at(
        &self,
        _: &McpGenerationRef,
        _: u64,
    ) -> Result<bool, RunError> {
        Ok(self.slots.0.lock().unwrap().root != Some(false))
    }
    fn relay_authority(&self) -> Weak<dyn McpGenerationAuthority> {
        let authority: Arc<dyn McpGenerationAuthority> = self.slots.clone();
        Arc::downgrade(&authority)
    }
    fn execution_backend(
        &self,
        _: &str,
    ) -> Result<Option<awaken_runtime_contract::resolved::Backend>, RunError> {
        Ok(self
            .platform
            .as_ref()
            .map(|_| awaken_runtime_contract::resolved::Backend::from_ref("acp:codex")))
    }
    fn supports_secret_egress_without_bypass(&self, _: &str) -> Result<bool, RunError> {
        Ok(self.platform.is_some())
    }
    fn frozen_network(
        &self,
        _: &str,
    ) -> Result<Option<awaken_provisioning_contract::NetworkPolicy>, RunError> {
        Ok(None)
    }
    fn acp_client_injection_capabilities(
        &self,
        _: &PinnedCredentialMaterializer,
        _: &str,
        _: bool,
    ) -> Result<Option<awaken_credential_contract::CredentialRealizationCapabilities>, RunError>
    {
        Ok(None)
    }
    async fn resident_environment(
        &self,
        _: &str,
    ) -> Result<Option<Arc<awaken_session_environment_contract::SessionEnvironment>>, RunError>
    {
        Ok(None)
    }
    fn frozen_agent(&self, _: &str) -> Result<Option<String>, RunError> {
        Ok(Some("agent".into()))
    }
    async fn realize_environment(&self, _: &str, _: &str) -> Result<(), RunError> {
        if let Some(gate) = &self.realization_gate {
            gate.pause().await;
        }
        Err(RunError::unavailable_classified(
            "fixture_realization_pending",
            "exact original preparation remains pending",
        ))
    }
}

fn credentials(context: &Context) -> McpCredentialMaterialization<'_> {
    match &context.platform {
        Some(inputs) => McpCredentialMaterialization {
            credentials: Some(&inputs.materializer),
            platform_egress: Some(inputs.provisioner.as_ref()),
            refresh_factory: None,
        },
        None => McpCredentialMaterialization::default(),
    }
}

fn request(url: &str) -> StageMcpAttachment {
    StageMcpAttachment {
        workspace_id: "workspace".into(),
        generation: crate::test_mcp::mcp_fence("session", "docs", 1, "runtime", 1, u64::MAX),
        realization_id: "realization".into(),
        stage_idempotency_key: "stage".into(),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http(url).unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    }
}

fn stage(
    mcp: McpSessions,
    slots: Arc<Slots>,
    request: StageMcpAttachment,
) -> tokio::task::JoinHandle<
    Result<awaken_session_configuration_contract::mcp::McpRealizationReceipt, RunError>,
> {
    let context = Arc::new(Context {
        slots: slots.clone(),
        platform: None,
        realization_gate: None,
    });
    tokio::spawn(async move {
        mcp.stage_assigned_attachment(&slots, context, credentials, request)
            .await
    })
}

#[tokio::test]
async fn native_http_preparation_is_owned_before_io_and_drain_joins_its_original_activity() {
    let slots = Slots::live();
    let mcp = McpSessions::new(None, awaken_time::system_clock());
    let gate = Arc::new(crate::test_mcp::InitializeGate::new());
    let (url, seen) = crate::test_mcp::start_with_initialize_gate(gate.clone()).await;
    let request = request(&url);
    let pending = stage(mcp.clone(), slots.clone(), request.clone());
    gate.entered().await;
    let original = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert_eq!(original.state, McpProjectionState::Staging);
    assert!(original.staging.is_some());
    let drain = mcp.drain_selected_projection(slots.as_ref(), &original);
    tokio::pin!(drain);
    assert!(futures_util::poll!(drain.as_mut()).is_pending());
    assert_eq!(
        mcp.projection(slots.as_ref(), &request.generation)
            .unwrap()
            .unwrap()
            .state,
        McpProjectionState::Draining
    );
    gate.release();
    assert_eq!(
        pending.await.unwrap().unwrap_err().code,
        "mcp_generation_draining"
    );
    drain.await.unwrap();
    let removed = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert_eq!(removed.state, McpProjectionState::Removed);
    assert!(removed.native_wiring.is_none());
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "initialize")
            .count(),
        1
    );
}

#[tokio::test]
async fn cancelling_a_native_waiter_retains_the_running_preparation_until_drain_can_join_it() {
    let slots = Slots::live();
    let mcp = McpSessions::new(None, awaken_time::system_clock());
    let gate = Arc::new(crate::test_mcp::InitializeGate::new());
    let (url, _) = crate::test_mcp::start_with_initialize_gate(gate.clone()).await;
    let request = request(&url);
    let pending = stage(mcp.clone(), slots.clone(), request.clone());
    gate.entered().await;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    let original = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert_eq!(original.state, McpProjectionState::Staging);
    let drain = mcp.drain_selected_projection(slots.as_ref(), &original);
    tokio::pin!(drain);
    assert!(futures_util::poll!(drain.as_mut()).is_pending());
    gate.release();
    drain.await.unwrap();
    assert_eq!(
        mcp.projection(slots.as_ref(), &request.generation)
            .unwrap()
            .unwrap()
            .state,
        McpProjectionState::Removed
    );
}

#[tokio::test]
async fn a_late_native_connection_never_publishes_into_the_same_named_replacement_allocation() {
    let slots = Slots::live();
    let clock = awaken_time::system_clock();
    let mcp = McpSessions::new(None, clock.clone());
    let gate = Arc::new(crate::test_mcp::InitializeGate::new());
    let (url, _) = crate::test_mcp::start_with_initialize_gate(gate.clone()).await;
    let request = request(&url);
    let pending = stage(mcp.clone(), slots.clone(), request.clone());
    gate.entered().await;
    slots.0.lock().unwrap().state = SessionMcpState::default();
    let replacement =
        McpStagingOwner::begin(&slots, &request, StagingAdmission::Assigned, clock.as_ref())
            .unwrap();
    let allocation = replacement.selection().drain.clone();
    gate.release();
    assert_eq!(
        pending.await.unwrap().unwrap_err().code,
        "mcp_generation_owner_changed"
    );
    let retained = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&allocation, &retained.drain));
    assert_eq!(retained.state, McpProjectionState::Staging);
    assert!(retained.server.is_none());
    replacement
        .authorize(StagingAdmission::Assigned, clock.as_ref())
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_stuck_preparation_times_out_without_forging_removed_and_cleanup_can_retry() {
    let slots = Slots::live();
    let clock = awaken_time::system_clock();
    let mcp = McpSessions::new(None, clock.clone());
    let request = request("http://127.0.0.1:1/mcp");
    let owner =
        McpStagingOwner::begin(&slots, &request, StagingAdmission::Assigned, clock.as_ref())
            .unwrap();
    let error = mcp
        .drain_selected_projection(slots.as_ref(), owner.selection())
        .await
        .unwrap_err();
    assert_eq!(error.code, "mcp_generation_staging_timeout");
    let retained = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert_eq!(retained.state, McpProjectionState::Draining);
    assert!(retained.staging.is_some());
    drop(owner);
    mcp.drain_selected_projection(slots.as_ref(), &retained)
        .await
        .unwrap();
    assert_eq!(
        mcp.projection(slots.as_ref(), &request.generation)
            .unwrap()
            .unwrap()
            .state,
        McpProjectionState::Removed
    );
}

struct UnusedReaper;
#[async_trait::async_trait]
impl awaken_session_environment_contract::ProcessReaper for UnusedReaper {
    async fn reap(
        &self,
        _: &dyn awaken_provisioning_contract::ProcessHandle,
        _: std::time::Duration,
    ) -> Result<bool, String> {
        panic!("a rejected Environment must never spawn an MCP process")
    }
}

#[tokio::test]
async fn stdio_preserves_the_original_environment_failure_kind_and_code() {
    let slots = Slots::live();
    let mut mcp = McpSessions::new(None, awaken_time::system_clock());
    mcp.set_process_reaper(Arc::new(UnusedReaper));
    let mut request = request("http://127.0.0.1:1/mcp");
    request.target =
        awaken_agent_contract::McpTarget::sandbox_stdio("fixture-command", vec![]).unwrap();
    let error = stage(mcp.clone(), slots.clone(), request.clone())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, "fixture_realization_pending");
    assert_eq!(error.kind, awaken_session_types::RunErrorKind::Unavailable);
    let removed = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert_eq!(removed.state, McpProjectionState::Removed);
    assert!(removed.mcp_process.is_none());
}

#[tokio::test]
async fn invalid_credential_binding_leaves_no_preparation_or_tombstone() {
    let slots = Slots::live();
    let mcp = McpSessions::new(None, awaken_time::system_clock());
    let mut request = request("http://127.0.0.1:1/mcp");
    request.selected_plaintext_holder = Some(awaken_credential_contract::PlaintextHolder::new(
        awaken_credential_contract::PlaintextBoundary::Worker,
        "fixture.worker",
    ));
    let error = stage(mcp.clone(), slots.clone(), request.clone())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, "mcp_credential_binding_invalid");
    assert!(
        mcp.projection(slots.as_ref(), &request.generation)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn missing_stdio_reaper_is_rejected_before_environment_preparation() {
    let slots = Slots::live();
    let mcp = McpSessions::new(None, awaken_time::system_clock());
    let mut request = request("http://127.0.0.1:1/mcp");
    request.target =
        awaken_agent_contract::McpTarget::sandbox_stdio("fixture-command", vec![]).unwrap();
    let error = stage(mcp.clone(), slots.clone(), request.clone())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, "mcp_generation_process_reaper_missing");
    assert!(
        mcp.projection(slots.as_ref(), &request.generation)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn cancelled_stdio_waiter_retains_environment_preparation_until_the_original_drain_settles() {
    let slots = Slots::live();
    let mut mcp = McpSessions::new(None, awaken_time::system_clock());
    mcp.set_process_reaper(Arc::new(UnusedReaper));
    let gate = Arc::new(crate::test_mcp::InitializeGate::new());
    let context = Arc::new(Context {
        slots: slots.clone(),
        platform: None,
        realization_gate: Some(gate.clone()),
    });
    let mut request = request("http://127.0.0.1:1/mcp");
    request.target =
        awaken_agent_contract::McpTarget::sandbox_stdio("fixture-command", vec![]).unwrap();
    let sessions = mcp.clone();
    let original_slots = slots.clone();
    let input = request.clone();
    let waiter = tokio::spawn(async move {
        sessions
            .stage_assigned_attachment(&original_slots, context, credentials, input)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), gate.entered())
        .await
        .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    let original = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert_eq!(original.state, McpProjectionState::Staging);
    let drain = mcp.drain_selected_projection(slots.as_ref(), &original);
    tokio::pin!(drain);
    assert!(futures_util::poll!(drain.as_mut()).is_pending());
    gate.release();
    drain.await.unwrap();
    let removed = mcp
        .projection(slots.as_ref(), &request.generation)
        .unwrap()
        .unwrap();
    assert_eq!(removed.state, McpProjectionState::Removed);
    assert!(removed.mcp_process.is_none());
    assert!(removed.native_wiring.is_none());
}
