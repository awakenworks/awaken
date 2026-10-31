//! Shared fakes for this crate's and the Runtime Host's Session environment
//! tests: a scripted container provider and environment, finished and
//! unreapable processes, Hand factories, and the canonical test spec.

#![allow(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel as AgentChannelType;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::{IsolationClass, NetworkPolicy, ResourceLimits, SandboxSpec};
use awaken_sandbox_container::ContainerHandleCodec as _;
use awaken_sandbox_container::{
    PublishedSandboxControlService, SandboxControlPublishError, SandboxControlService,
    SandboxControlServiceKind, SandboxControlServicePublisher,
};
use awaken_session_environment_contract::HandExecutorFactory;
use awaken_tool_contract::tool::ToolCall;
use awaken_tool_contract::tool::ToolExecutor;
use tokio::io::AsyncReadExt;

#[derive(Default)]
pub struct FakeContainerProvider {
    /// Readiness is fresh provider state, independent of its realized handles.
    pub readiness_error: awaken_sync::LeafLock<Option<pc::SandboxError>>,
    pub readiness_calls: std::sync::atomic::AtomicUsize,
    pub checkpoint_formats: awaken_sync::LeafLock<Vec<String>>,
    pub observation_calls: std::sync::atomic::AtomicUsize,
    /// One typed observation override; None retains the original unavailable
    /// read. Filesystem test bytes no longer double as provider control state.
    pub observation:
        awaken_sync::LeafLock<Option<Result<pc::SandboxObservation, pc::SandboxError>>>,
    pub observations: awaken_sync::LeafLock<
        Vec<(
            awaken_provisioning_contract::SandboxObservationPurpose,
            pc::SandboxEffectFence,
        )>,
    >,
    /// A scripted refusal in the original shared Create entry, before a handle exists.
    pub create_error: awaken_sync::LeafLock<Option<pc::SandboxError>>,
    pub adopts: std::sync::atomic::AtomicUsize,
    pub creates: std::sync::atomic::AtomicUsize,
    pub disposals: Arc<std::sync::atomic::AtomicUsize>,
    pub specs: awaken_sync::LeafLock<Vec<pc::SandboxSpec>>,
    pub renews: Arc<std::sync::atomic::AtomicUsize>,
    pub hand_spawns: Arc<std::sync::atomic::AtomicUsize>,
    pub resident_channel_opens: Arc<std::sync::atomic::AtomicUsize>,
    pub fail_hand_spawn_at: Arc<std::sync::atomic::AtomicUsize>,
    pub shared: Arc<awaken_sync::LeafLock<std::collections::HashMap<String, Vec<u8>>>>,
}

pub struct FakeContainer {
    pub handle: pc::SandboxHandle,
    pub disposals: Arc<std::sync::atomic::AtomicUsize>,
    pub renews: Arc<std::sync::atomic::AtomicUsize>,
    pub hand_spawns: Arc<std::sync::atomic::AtomicUsize>,
    pub resident_channel_opens: Arc<std::sync::atomic::AtomicUsize>,
    pub fail_hand_spawn_at: Arc<std::sync::atomic::AtomicUsize>,
    pub shared: Arc<awaken_sync::LeafLock<std::collections::HashMap<String, Vec<u8>>>>,
}

pub struct DoneProcess {
    pub id: String,
    pub code: i32,
}

pub struct UnreapableProcess;

pub struct SlowReapProcess;

impl DoneProcess {
    pub fn success(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            code: 0,
        }
    }

    pub fn exited(id: impl Into<String>, code: i32) -> Self {
        Self {
            id: id.into(),
            code,
        }
    }
}

pub struct FakeHandExecutorFactory;

pub struct FakeHandExecutor;

impl HandExecutorFactory for FakeHandExecutorFactory {
    fn bind(
        &self,
        _channel: Box<dyn AgentChannelType>,
        _operation_scope: &str,
        _recovery: awaken_tool_contract::tool::ToolRecoveryCapability,
    ) -> awaken_session_environment_contract::HandExecutorBinding {
        awaken_session_environment_contract::HandExecutorBinding::execution_only(Arc::new(
            FakeHandExecutor,
        ))
    }
}

#[async_trait]
impl ToolExecutor for FakeHandExecutor {
    async fn invoke(
        &self,
        call: &ToolCall,
    ) -> Result<awaken_tool_contract::tool::ToolOutput, awaken_tool_contract::tool::ToolError> {
        Ok(awaken_tool_contract::tool::ToolOutput::ok(
            call.call_id.clone(),
            "bound-hand-ok",
        ))
    }
}

#[async_trait]
impl pc::ProcessHandle for DoneProcess {
    fn id(&self) -> &str {
        &self.id
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        Ok(pc::ExitStatus {
            code: Some(self.code),
            signaled: false,
        })
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        Ok(Some(self.wait().await?))
    }

    async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
        Ok(())
    }
}

#[async_trait]
impl pc::ProcessHandle for UnreapableProcess {
    fn id(&self) -> &str {
        "unreapable-hand"
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        std::future::pending().await
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        Ok(None)
    }

    async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::backend(
            "signal",
            "scripted signal failure",
        ))
    }
}

#[async_trait]
impl pc::ProcessHandle for SlowReapProcess {
    fn id(&self) -> &str {
        "slow-reap-hand"
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        std::future::pending().await
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        Ok(None)
    }

    async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
        Ok(())
    }
}

#[async_trait]
impl awaken_provisioning_contract::SandboxReadiness for FakeContainerProvider {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        // Synthetic provider admission for lifecycle tests. Actual containment
        // is validated by the separate Namespace/Docker/Podman service journeys.
        pc::SandboxCapabilities {
            isolation: pc::IsolationClass::Container,
            tool_transparent: true,
            path_fidelity: true,
            enforced_readonly: true,
            network_isolation: false,
            enforced_network_allowlist: false,
            secret_egress_substitution: false,
            resource_limits: false,
            custom_rootfs: false,
            package_provisioning: false,
            control_services: Default::default(),
            process_tree_supervision: false,
        }
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        self.checkpoint_formats.lock().clone()
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        self.readiness_calls.fetch_add(1, Ordering::SeqCst);
        match self.readiness_error.lock().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::SandboxProvider for FakeContainerProvider {
    type Environment = dyn awaken_provisioning_contract::SessionSandbox;

    async fn prewarm_images(
        &self,
        _specs: &[awaken_provisioning_contract::SandboxSpec],
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }

    fn adoption_layout(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<
        awaken_provisioning_contract::SandboxAdoptionLayout,
        awaken_provisioning_contract::SandboxError,
    > {
        awaken_sandbox_container::container_adoption_layout(handle)
    }

    fn validate_closed_observation(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
        observation: &awaken_provisioning_contract::SandboxObservation,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        awaken_sandbox_container::validate_container_closed_observation(handle, observation)
    }

    fn claim_for_effect<'a>(
        &'a self,
        spec: &'a awaken_provisioning_contract::SandboxSpec,
        effect: awaken_provisioning_contract::SandboxEffect<'a>,
        intent: awaken_provisioning_contract::SandboxRealizationIntent,
    ) -> awaken_provisioning_contract::ClaimedEffect<
        'a,
        std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
    > {
        let claim = awaken_provisioning_contract::EffectClaim::in_process(
            awaken_provisioning_contract::test_support::retain_abandoned_effect(spec.scope.clone()),
        );
        awaken_provisioning_contract::ClaimedEffect::new(claim, async move {
            intent.admit_under(effect)?;
            match effect {
                awaken_provisioning_contract::SandboxEffect::Unattached => {
                    self.create_environment(spec)
                }
                awaken_provisioning_contract::SandboxEffect::Fenced { fence, authority } => {
                    let effect_fence = fence;

                    // The fake allocates synchronously inside the original current-effect
                    // guard. It supplies no physical recovery or Kubernetes participant proof.
                    pc::with_current_sandbox_effect(effect_fence, authority, |_| Ok(()))?;
                    if !matches!(
                        intent,
                        awaken_provisioning_contract::SandboxRealizationIntent::Create
                    ) {
                        return Err(pc::SandboxError::unsupported(
                            "test double does not implement fenced Environment rebuild",
                        ));
                    }
                    pc::with_current_sandbox_effect(effect_fence, authority, |_| {
                        self.create_environment(spec)
                    })
                }
            }
        })
    }

    async fn observe(
        &self,
        adoption: awaken_provisioning_contract::SandboxAdoption<'_>,
        basis: pc::SandboxObservationBasis<'_>,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        self.observation_calls.fetch_add(1, Ordering::SeqCst);
        if let pc::SandboxObservationBasis::Effect { effect_fence, .. } = basis {
            self.observations
                .lock()
                .push((adoption.observation_purpose, effect_fence.clone()));
        }
        self.observation.lock().clone().unwrap_or_else(|| {
            Err(pc::SandboxError::transport(
                "fixture observation unavailable",
            ))
        })
    }

    async fn adopt_for_effect(
        &self,
        adoption: awaken_provisioning_contract::SandboxAdoption<'_>,
        effect: awaken_provisioning_contract::SandboxEffect<'_>,
    ) -> Result<
        std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
        awaken_provisioning_contract::SandboxError,
    > {
        match effect {
            awaken_provisioning_contract::SandboxEffect::Unattached => {
                self.adopts.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new(FakeContainer {
                    handle: adoption.handle.clone(),
                    disposals: self.disposals.clone(),
                    renews: self.renews.clone(),
                    hand_spawns: self.hand_spawns.clone(),
                    resident_channel_opens: self.resident_channel_opens.clone(),
                    fail_hand_spawn_at: self.fail_hand_spawn_at.clone(),
                    shared: self.shared.clone(),
                }))
            }
            awaken_provisioning_contract::SandboxEffect::Fenced { fence, authority } => {
                let _adoption = adoption;
                let _effect_fence = fence;
                let _authority = authority;

                Err(awaken_provisioning_contract::SandboxError::unsupported(
                    "test double does not implement fenced Environment adoption",
                ))
            }
        }
    }

    async fn prepare_terminal_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
        _expected_effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
        _terminal_effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        Option<std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced terminal preparation",
        ))
    }

    fn restoration(
        &self,
    ) -> awaken_provisioning_contract::SandboxRestoration<
        '_,
        std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
    > {
        awaken_provisioning_contract::SandboxRestoration::Unsupported
    }
}

impl awaken_provisioning_contract::SandboxInstallation for FakeContainerProvider {
    fn install_memory_mounter(
        &self,
        _mounter: std::sync::Arc<dyn awaken_provisioning_contract::MemoryMounter>,
    ) {
    }

    fn install_secret_broker(
        &self,
        _broker: std::sync::Arc<dyn awaken_provisioning_contract::SecretBroker>,
    ) {
    }
}

impl FakeContainerProvider {
    fn create_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn awaken_provisioning_contract::SessionSandbox>, pc::SandboxError> {
        self.creates
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.specs.lock().push(spec.clone());
        if let Some(error) = self.create_error.lock().clone() {
            return Err(error);
        }
        Ok(Arc::new(FakeContainer {
            handle: current_fake_container_handle(spec),
            disposals: self.disposals.clone(),
            renews: self.renews.clone(),
            hand_spawns: self.hand_spawns.clone(),
            resident_channel_opens: self.resident_channel_opens.clone(),
            fail_hand_spawn_at: self.fail_hand_spawn_at.clone(),
            shared: self.shared.clone(),
        }))
    }
}

#[async_trait]
impl SandboxControlServicePublisher for FakeContainer {
    async fn publish_sandbox_control_service(
        &self,
        _kind: SandboxControlServiceKind,
        _service: Arc<dyn SandboxControlService>,
    ) -> Result<Box<dyn PublishedSandboxControlService>, SandboxControlPublishError> {
        // This fixture declares no control-service topology, so publication must
        // fail closed instead of manufacturing a successful provider lease.
        Err(SandboxControlPublishError)
    }
}

#[async_trait]
impl pc::Sandbox for FakeContainer {
    async fn prepare_disposal_for_effect(
        &self,
        _effect_fence: &pc::SandboxEffectFence,
        _authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<pc::SandboxEffectFence, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "test double does not prepare disposal",
        ))
    }

    async fn dispose_unattached(&self) -> Result<(), pc::SandboxError> {
        self.dispose().await
    }

    fn id(&self) -> &str {
        &self.handle.sandbox_id
    }

    fn handle(&self) -> pc::SandboxHandle {
        self.handle.clone()
    }

    async fn spawn(
        &self,
        command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        if command.argv.iter().any(|part| part.contains("marker")) {
            self.shared
                .lock()
                .insert("marker".into(), b"shared-container-state".to_vec());
        }
        let repository_import = command.argv.iter().any(|part| part == "awaken-repo-clone");
        let exit_code =
            if repository_import && self.shared.lock().contains_key("__fail_repository_import") {
                23
            } else {
                0
            };
        Ok(Box::new(DoneProcess::exited("native-exec", exit_code)))
    }

    async fn attach(
        &self,
        requirement: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        if let pc::MountSource::InlineBytes { contents, .. } = &requirement.source
            && awaken_container_contract::live_input_relative_path(&requirement.mount_path)
                .is_some()
            && requirement.access == pc::MountAccess::ReadOnly
        {
            self.shared
                .lock()
                .insert(requirement.mount_path.clone(), contents.clone());
            return Ok(pc::RealizedMount {
                mount_id: requirement.mount_id,
                mount_path: requirement.mount_path,
                access: requirement.access,
                realization: pc::Realization::Bind,
                content_hash: None,
            });
        }
        Err(pc::SandboxError::unsupported("unsupported"))
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        Ok(Vec::new())
    }

    async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::not_found("missing"))
    }

    fn realized(&self) -> &[pc::RealizedMount] {
        &[]
    }

    async fn process(&self, id: &str) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        Ok(Box::new(DoneProcess::success(id)))
    }

    async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        Ok(pc::SandboxStatus::Ready)
    }

    async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
        self.renews
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn dispose_for_effect(
        &self,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        pc::with_current_sandbox_effect(authorization.effect_fence(), authority, |_| Ok(()))?;
        self.dispose().await
    }
}

impl FakeContainer {
    // Relocated from the Sandbox/SandboxProvider trait: the unfenced ports
    // are no longer a trait obligation or a reachable escape hatch. They stay
    // as this type's own implementation detail, used by its fenced edges.
    pub async fn dispose(&self) -> Result<(), pc::SandboxError> {
        if self.shared.lock().contains_key("__fail_dispose") {
            return Err(pc::SandboxError::backend(
                "dispose",
                "injected disposal unavailable",
            ));
        }
        self.disposals
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

pub fn spec() -> SandboxSpec {
    SandboxSpec {
        process_tree_supervision: false,
        scope: "session-env".into(),
        isolation: IsolationClass::Workdir,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: NetworkPolicy::Unrestricted,
        outputs_path: "/outputs".into(),
        requests: Default::default(),
        limits: ResourceLimits::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        control_services: Default::default(),
        lease_ttl_secs: None,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    }
}

pub fn current_fake_container_handle(spec: &pc::SandboxSpec) -> pc::SandboxHandle {
    let realization_fingerprint = pc::SandboxRealizationFingerprint::from_spec(spec);
    let owned_paths = spec
        .mounts
        .iter()
        .map(|mount| mount.mount_path.clone())
        .collect::<Vec<_>>();
    pc::SandboxHandle::container_v2(
        "session-container",
        awaken_sandbox_container::ContainerSandboxHandleV2 {
            previous: awaken_sandbox_container::ContainerSandboxHandleV1 {
                container_id: "container-session-container".into(),
                outputs_path: spec.outputs_path.clone(),
                base_env: spec.env.clone(),
                live_input_projection: false,
                continuation_excluded_paths: owned_paths.clone(),
                runtime_handle: None,
                sandbox_control_incarnation: None,
                control_services: spec.control_services.clone(),
            },
            adoption_fingerprint: realization_fingerprint.clone(),
            realization_fingerprint,
            owned_paths,
        },
    )
}

#[derive(Clone, Copy)]
pub enum ScriptedHandOutcome {
    Success,
    UnavailableBeforeDispatch,
    Indeterminate,
}

pub struct ScriptedHandExecutor {
    pub outcome: ScriptedHandOutcome,
}

#[async_trait]
impl ToolExecutor for ScriptedHandExecutor {
    async fn invoke(
        &self,
        call: &ToolCall,
    ) -> Result<awaken_tool_contract::tool::ToolOutput, awaken_tool_contract::tool::ToolError> {
        match self.outcome {
            ScriptedHandOutcome::Success => Ok(awaken_tool_contract::tool::ToolOutput::ok(
                &call.call_id,
                "recovered",
            )),
            ScriptedHandOutcome::UnavailableBeforeDispatch => Err(
                awaken_tool_contract::tool::ToolError::UnavailableBeforeDispatch(
                    "expired attached exec".into(),
                ),
            ),
            ScriptedHandOutcome::Indeterminate => {
                Err(awaken_tool_contract::tool::ToolError::Indeterminate(
                    "indeterminate: hand connection lost during dispatch".into(),
                ))
            }
        }
    }
}

pub struct ScriptedHandFactory {
    pub outcomes: awaken_sync::LeafLock<std::collections::VecDeque<ScriptedHandOutcome>>,
    pub binds: std::sync::atomic::AtomicUsize,
}

impl ScriptedHandFactory {
    pub fn new(outcomes: impl IntoIterator<Item = ScriptedHandOutcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: awaken_sync::LeafLock::new(outcomes.into_iter().collect()),
            binds: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

impl HandExecutorFactory for ScriptedHandFactory {
    fn bind(
        &self,
        _channel: Box<dyn AgentChannelType>,
        _operation_scope: &str,
        _recovery: awaken_tool_contract::tool::ToolRecoveryCapability,
    ) -> awaken_session_environment_contract::HandExecutorBinding {
        self.binds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        awaken_session_environment_contract::HandExecutorBinding::execution_only(Arc::new(
            ScriptedHandExecutor {
                outcome: self
                    .outcomes
                    .lock()
                    .pop_front()
                    .unwrap_or(ScriptedHandOutcome::Success),
            },
        ))
    }
}

pub fn restart_disposal_authorization(
    prepared: pc::SandboxEffectFence,
    label: &str,
) -> pc::SandboxDisposalAuthorization {
    let preparation = pc::SandboxDisposalPreparation::new(prepared, label).unwrap();
    // The exact renewal of the prepared lease under the disposal operation.
    let effect = pc::SandboxEffectFence::for_operation(
        preparation.operation_id().unwrap(),
        preparation.prepared_effect_fence(),
    )
    .unwrap();
    preparation.authorize(effect).unwrap()
}

#[async_trait]
impl awaken_provisioning_contract::SessionSandbox for FakeContainer {
    async fn owned_process_binding(&self) -> Result<Option<String>, pc::SandboxError> {
        let shared = self.shared.lock();
        if shared.contains_key("__process_binding_error") {
            return Err(pc::SandboxError::backend("process_binding", "unobservable"));
        }
        Ok(shared.get("__process_binding").map(|bytes| {
            String::from_utf8(bytes.clone()).expect("fixture process reference is UTF-8")
        }))
    }

    async fn checkpoint_upload(
        &self,
        _access: awaken_provisioning_contract::SandboxCheckpointAccess<'_>,
        _mutation: awaken_provisioning_contract::SandboxCheckpointMutation<'_>,
    ) -> Result<
        Option<awaken_provisioning_contract::SandboxCheckpointUpload>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test container environment does not implement a fenced checkpoint participant",
        ))
    }

    async fn remove_live_input_path(
        &self,
        _path: &str,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "late mount removal is unsupported on this test container",
        ))
    }

    fn record_owned_path(&self, path: &str) -> Result<(), pc::SandboxError> {
        self.shared
            .lock()
            .entry(format!("__owned_path:{path}"))
            .or_default();
        Ok(())
    }

    async fn open_agent_channel(&self) -> Result<Box<dyn AgentChannelType>, pc::SandboxError> {
        self.resident_channel_opens
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (ours, mut theirs) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut sink = Vec::new();
            let _ = theirs.read_to_end(&mut sink).await;
        });
        Ok(Box::new(ours))
    }

    async fn spawn_agent_process(
        &self,
        command: pc::Command,
    ) -> Result<awaken_provisioning_contract::AgentProcess, pc::SandboxError> {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let repository_export = command.argv.iter().any(|part| part == "awaken-repo-push");
        let repository_import = command.argv.iter().any(|part| part == "awaken-repo-clone");
        if command.argv.iter().any(|part| part == "--stdio") {
            // P7 Container already exposes /mnt: preserve its deployed Hand
            // CLI rather than requiring the new local projection option. All
            // cold/reacquired container Hand cases share this contract oracle.
            assert_eq!(&command.argv[1..], &["hand", "--stdio"]);
            let spawn = self
                .hand_spawns
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if self
                .fail_hand_spawn_at
                .load(std::sync::atomic::Ordering::SeqCst)
                == spawn
            {
                return Err(pc::SandboxError::backend(
                    "spawn_agent_process",
                    "scripted Hand spawn failure",
                ));
            }
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut theirs = theirs;
                let mut sink = Vec::new();
                let _ = theirs.read_to_end(&mut sink).await;
            });
        } else if repository_import {
            let observation = self
                .shared
                .lock()
                .get("__repository_import_observation")
                .cloned()
                .unwrap_or_default();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut theirs = theirs;
                let _ = theirs.write_all(&observation).await;
            });
        } else if command.argv.iter().any(|part| part.contains("cat")) {
            let bytes = self
                .shared
                .lock()
                .get("marker")
                .cloned()
                .unwrap_or_default();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut theirs = theirs;
                let _ = theirs.write_all(&bytes).await;
            });
        } else if repository_export {
            let observation = self
                .shared
                .lock()
                .get("__repository_export_observation")
                .cloned();
            if let Some(observation) = observation {
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let mut theirs = theirs;
                    let _ = theirs.write_all(&observation).await;
                });
            } else {
                drop(theirs);
            }
        } else {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut theirs = theirs;
                let mut bytes = Vec::new();
                let _ = theirs.read_to_end(&mut bytes).await;
            });
        }
        let exit_code = if repository_import
            && self.shared.lock().contains_key("__fail_repository_import")
        {
            23
        } else if repository_export && self.shared.lock().contains_key("__fail_repository_export") {
            19
        } else if repository_export
            && self
                .shared
                .lock()
                .contains_key("__repository_export_observation")
        {
            42
        } else {
            0
        };
        let unreapable = command.argv.iter().any(|part| part == "--stdio")
            && self.shared.lock().contains_key("__unreapable_hand");
        let slow_reap = command.argv.iter().any(|part| part == "--stdio")
            && self.shared.lock().contains_key("__slow_reap_hand");
        let process: Box<dyn pc::ProcessHandle> = if unreapable {
            Box::new(UnreapableProcess)
        } else if slow_reap {
            Box::new(SlowReapProcess)
        } else {
            Box::new(DoneProcess::exited("container-exec", exit_code))
        };
        Ok(awaken_provisioning_contract::AgentProcess {
            process,
            channel: Box::new(ours),
        })
    }

    async fn read_files(
        &self,
        _root: &str,
    ) -> Result<Vec<awaken_provisioning_contract::SandboxFile>, pc::SandboxError> {
        if self.shared.lock().contains_key("__fail_skill_read") {
            return Err(pc::SandboxError::backend(
                "read_files",
                "scripted Skill read outage",
            ));
        }
        Ok(self
            .shared
            .lock()
            .iter()
            .map(|(path, bytes)| awaken_provisioning_contract::SandboxFile {
                path: path.clone(),
                bytes: bytes.clone(),
            })
            .collect())
    }
}

/// Binds no Hand tool: a test that never dispatches a Session Hand tool.
pub struct UnusedHandExecutorFactory;

impl HandExecutorFactory for UnusedHandExecutorFactory {
    fn bind(
        &self,
        _channel: Box<dyn AgentChannelType>,
        _operation_scope: &str,
        _recovery: awaken_tool_contract::tool::ToolRecoveryCapability,
    ) -> awaken_session_environment_contract::HandExecutorBinding {
        panic!("this test does not dispatch a Session Hand tool")
    }
}

/// Test access to the sandbox adapter behind a Session environment's role
/// handles, for observability the roles deliberately do not offer.
pub trait SandboxEnvironmentAccess {
    fn sandbox_environment(&self) -> &crate::SandboxEnvironment;
}

impl SandboxEnvironmentAccess for awaken_session_environment_contract::SessionEnvironment {
    fn sandbox_environment(&self) -> &crate::SandboxEnvironment {
        self.origin()
            .downcast_ref()
            .expect("a sandbox-backed Session environment")
    }
}

/// Test access to the sandbox adapter behind a Session environment provider's
/// role handles.
pub trait SandboxEnvironmentProviderAccess {
    fn sandbox_provider(&self) -> &crate::SandboxEnvironmentProvider;
}

impl SandboxEnvironmentProviderAccess
    for awaken_session_environment_contract::SandboxProviderRoles
{
    fn sandbox_provider(&self) -> &crate::SandboxEnvironmentProvider {
        self.origin()
            .downcast_ref()
            .expect("a sandbox-backed Session environment provider")
    }
}

/// Relocate a sandbox-backed provider under `base`, so a durable fixture's
/// environments survive a restart at a stable root. A provider this crate did
/// not build is kept as installed.
#[must_use]
pub fn relocated_provider(
    provider: &awaken_session_environment_contract::SandboxProviderRoles,
    base: impl Into<std::path::PathBuf>,
) -> awaken_session_environment_contract::SandboxProviderRoles {
    let base = base.into();
    provider
        .origin()
        .downcast_ref::<crate::SandboxEnvironmentProvider>()
        .map_or_else(
            || provider.clone(),
            |sandbox| sandbox.at_root(base).into_provider_roles(),
        )
}

/// The Hand companion or scoped launcher the portability gate built, by its
/// environment variable.
#[expect(
    clippy::disallowed_methods,
    reason = "test support locates the Hand companion the portability gate built from the environment"
)]
pub fn deployment_test_binary(key: &str) -> std::path::PathBuf {
    let path =
        std::path::PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("required {key}")));
    assert!(path.is_absolute() && path.is_file(), "invalid {key}");
    path
}

// Fixture compositions choose their own clock; product constructors require
// the process clock explicitly and never select an ambient clock.
impl crate::SandboxEnvironmentProvider {
    pub fn container(
        provider: Arc<
            dyn awaken_provisioning_contract::SandboxProvider<
                    Environment = dyn awaken_provisioning_contract::SessionSandbox,
                >,
        >,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
    ) -> Self {
        Self::container_with_hand_idle(
            provider,
            extra_mounts,
            hand_factory,
            hand_bin,
            std::time::Duration::ZERO,
        )
    }

    pub fn container_with_hand_idle(
        provider: Arc<
            dyn awaken_provisioning_contract::SandboxProvider<
                    Environment = dyn awaken_provisioning_contract::SessionSandbox,
                >,
        >,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        hand_idle_after: std::time::Duration,
    ) -> Self {
        Self::container_with_hand_idle_and_residency(
            provider,
            extra_mounts,
            hand_factory,
            hand_bin,
            hand_idle_after,
            awaken_session_environment_contract::ContainerHandResidency::AttachedExec,
            awaken_time::system_clock(),
        )
    }
}
