//! Session environment tests, split by topic; the shared fake providers live here.
use super::*;
use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::{
    IsolationClass, NetworkPolicy, ResourceLimits, SandboxProvider, SandboxSpec,
};

use awaken_run_executor_acp::AgentChannelType;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::ToolExecutor;
use awaken_sandbox_container::ContainerEnvironmentProvider as _;
use awaken_sandbox_container::ContainerHandleCodec as _;
use awaken_sandbox_container::{
    PublishedSandboxControlService, SandboxControlPublishError, SandboxControlService,
    SandboxControlServiceKind, SandboxControlServicePublisher,
};

use awaken_sandbox_local::FilesystemHandleCodec as _;
use awaken_sandbox_local::NamespaceProvider;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::AsyncReadExt;

mod container_environment;
mod container_projection;
mod container_transfer;
mod execution_domain;
mod memory_replay;
mod session_hand;
mod shared_environment;

#[derive(Default)]
struct FakeContainerProvider {
    observations: std::sync::Mutex<
        Vec<(
            awaken_sandbox_container::ContainerObservationPurpose,
            pc::SandboxEffectFence,
        )>,
    >,
    adopts: std::sync::atomic::AtomicUsize,
    creates: std::sync::atomic::AtomicUsize,
    disposals: Arc<std::sync::atomic::AtomicUsize>,
    specs: std::sync::Mutex<Vec<pc::SandboxSpec>>,
    renews: Arc<std::sync::atomic::AtomicUsize>,
    hand_spawns: Arc<std::sync::atomic::AtomicUsize>,
    resident_channel_opens: Arc<std::sync::atomic::AtomicUsize>,
    fail_hand_spawn_at: Arc<std::sync::atomic::AtomicUsize>,
    shared: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

struct FakeContainer {
    handle: pc::SandboxHandle,
    disposals: Arc<std::sync::atomic::AtomicUsize>,
    renews: Arc<std::sync::atomic::AtomicUsize>,
    hand_spawns: Arc<std::sync::atomic::AtomicUsize>,
    resident_channel_opens: Arc<std::sync::atomic::AtomicUsize>,
    fail_hand_spawn_at: Arc<std::sync::atomic::AtomicUsize>,
    shared: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

pub(super) struct DoneProcess {
    id: String,
    code: i32,
}

pub(super) struct UnreapableProcess;

struct SlowReapProcess;

impl DoneProcess {
    pub(super) fn success(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            code: 0,
        }
    }

    fn exited(id: impl Into<String>, code: i32) -> Self {
        Self {
            id: id.into(),
            code,
        }
    }
}

struct FakeHandExecutorFactory;

pub(super) struct FakeHandExecutor;

impl HandExecutorFactory for FakeHandExecutorFactory {
    fn bind(
        &self,
        _channel: Box<dyn AgentChannelType>,
        _operation_scope: &str,
        _recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
    ) -> Arc<dyn ToolExecutor> {
        Arc::new(FakeHandExecutor)
    }
}

#[async_trait]
impl ToolExecutor for FakeHandExecutor {
    async fn invoke(
        &self,
        call: &ToolCall,
    ) -> Result<awaken_runtime_contract::tool::ToolOutput, awaken_runtime_contract::tool::ToolError>
    {
        Ok(awaken_runtime_contract::tool::ToolOutput::ok(
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
impl awaken_sandbox_container::ContainerEnvironmentProvider for FakeContainerProvider {
    async fn create_environment_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _effect_fence: &awaken_sandbox_container::ContainerEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
        _intent: awaken_sandbox_container::ContainerRealizationIntent,
    ) -> Result<
        std::sync::Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced Environment creation",
        ))
    }
    async fn adopt_environment_for_effect(
        &self,
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        _effect_fence: &awaken_sandbox_container::ContainerEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        std::sync::Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced Environment adoption",
        ))
    }
    async fn prepare_terminal_environment_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
        _expected_effect_fence: Option<&awaken_sandbox_container::ContainerEffectFence>,
        _terminal_effect_fence: &awaken_sandbox_container::ContainerEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        Option<std::sync::Arc<dyn awaken_sandbox_container::ContainerEnvironment>>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced terminal preparation",
        ))
    }

    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
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

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    async fn observe_environment(
        &self,
        adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        basis: pc::SandboxObservationBasis<'_>,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        if let pc::SandboxObservationBasis::Effect { effect_fence, .. } = basis {
            self.observations
                .lock()
                .unwrap()
                .push((adoption.observation_purpose, effect_fence.clone()));
        }
        if self
            .shared
            .lock()
            .unwrap()
            .contains_key("__ready_observation")
        {
            Ok(pc::SandboxObservation::Ready)
        } else {
            Err(pc::SandboxError::transport(
                "fixture observation unavailable",
            ))
        }
    }

    async fn create_unattached_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError> {
        self.creates
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.specs.lock().unwrap().push(spec.clone());
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

    async fn adopt_unattached_environment(
        &self,
        adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError> {
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
                .unwrap()
                .insert("marker".into(), b"shared-container-state".to_vec());
        }
        let repository_import = command.argv.iter().any(|part| part == "awaken-repo-clone");
        let exit_code = if repository_import
            && self
                .shared
                .lock()
                .unwrap()
                .contains_key("__fail_repository_import")
        {
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
            && awaken_sandbox_container::live_input_relative_path(&requirement.mount_path).is_some()
            && requirement.access == pc::MountAccess::ReadOnly
        {
            self.shared
                .lock()
                .unwrap()
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
    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        self.disposals
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

pub(crate) fn spec() -> SandboxSpec {
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

fn current_fake_container_handle(spec: &pc::SandboxSpec) -> pc::SandboxHandle {
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
enum ScriptedHandOutcome {
    Success,
    UnavailableBeforeDispatch,
    Indeterminate,
}

struct ScriptedHandExecutor {
    outcome: ScriptedHandOutcome,
}

#[async_trait]
impl ToolExecutor for ScriptedHandExecutor {
    async fn invoke(
        &self,
        call: &ToolCall,
    ) -> Result<awaken_runtime_contract::tool::ToolOutput, awaken_runtime_contract::tool::ToolError>
    {
        match self.outcome {
            ScriptedHandOutcome::Success => Ok(awaken_runtime_contract::tool::ToolOutput::ok(
                &call.call_id,
                "recovered",
            )),
            ScriptedHandOutcome::UnavailableBeforeDispatch => Err(
                awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(
                    "expired attached exec".into(),
                ),
            ),
            ScriptedHandOutcome::Indeterminate => {
                Err(awaken_runtime_contract::tool::ToolError::Indeterminate(
                    "indeterminate: hand connection lost during dispatch".into(),
                ))
            }
        }
    }
}

struct ScriptedHandFactory {
    outcomes: std::sync::Mutex<std::collections::VecDeque<ScriptedHandOutcome>>,
    binds: std::sync::atomic::AtomicUsize,
}

impl ScriptedHandFactory {
    fn new(outcomes: impl IntoIterator<Item = ScriptedHandOutcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: std::sync::Mutex::new(outcomes.into_iter().collect()),
            binds: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

impl HandExecutorFactory for ScriptedHandFactory {
    fn bind(
        &self,
        _channel: Box<dyn AgentChannelType>,
        _operation_scope: &str,
        _recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
    ) -> Arc<dyn ToolExecutor> {
        self.binds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Arc::new(ScriptedHandExecutor {
            outcome: self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ScriptedHandOutcome::Success),
        })
    }
}

pub(super) fn restart_disposal_authorization(
    prepared: pc::SandboxEffectFence,
    label: &str,
) -> pc::SandboxDisposalAuthorization {
    let preparation = pc::SandboxDisposalPreparation::new(prepared, label).unwrap();
    let effect = pc::SandboxEffectFence::new(
        preparation.operation_id().unwrap(),
        "owner",
        "runtime",
        1,
        u64::MAX,
    )
    .unwrap();
    preparation.authorize(effect).unwrap()
}
