//! The single live execution environment owned by a Session.
//!
//! Host code depends on this capability object instead of retaining a concrete
//! sandbox in `SessionCtx`. Workdir is the first adapter; Namespace and Container
//! plug into the same owner without adding another Native/ACP lifecycle.

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_run_executor_acp::AgentChannelType;
use awaken_runtime_contract::tool::{RawTool, RawToolRegistry, ToolExecutor};
use awaken_sandbox_local::{LocalSandbox, NamespaceSandbox};

mod agent_sandbox;
mod container_files;
mod container_repositories;
mod container_skills;
mod provider;
mod repository_realizer;
mod session_files;
mod session_hand;
pub(crate) use agent_sandbox::AgentSandbox;
use container_skills::ContainerSkillCache;
pub(crate) use provider::SessionEnvironmentProvider;
use session_hand::HandProjectionUpdate;
use session_hand::SessionHandExecutor;

/// Session environment service that binds a live hand channel to the runtime's tool
/// executor. The framing implementation belongs to an outer startup crate;
/// this host owns only the Session lifecycle and never imports the relay adapter.
pub trait HandExecutorFactory: Send + Sync {
    fn bind(
        &self,
        channel: Box<dyn AgentChannelType>,
        operation_scope: &str,
        recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
    ) -> Arc<dyn ToolExecutor>;
}

#[cfg(test)]
pub(crate) struct UnusedHandExecutorFactory;

#[cfg(test)]
impl HandExecutorFactory for UnusedHandExecutorFactory {
    fn bind(
        &self,
        _channel: Box<dyn AgentChannelType>,
        _operation_scope: &str,
        _recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
    ) -> Arc<dyn ToolExecutor> {
        panic!("this test does not dispatch a Session Hand tool")
    }
}

/// One realized sandbox shared by every Run attempt in a Session.
pub(crate) enum SessionEnvironment {
    Workdir(Arc<LocalSandbox>),
    Namespace {
        sandbox: Arc<NamespaceSandbox>,
        hand: Arc<SessionHandExecutor>,
    },
    Container {
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        hand: Arc<SessionHandExecutor>,
        skills: Arc<ContainerSkillCache>,
        capabilities: pc::SandboxCapabilities,
    },
}

impl SessionEnvironment {
    fn sandbox(&self) -> &dyn pc::Sandbox {
        match self {
            Self::Workdir(sandbox) => sandbox.as_ref(),
            Self::Namespace { sandbox, .. } => sandbox.as_ref(),
            Self::Container { sandbox, .. } => sandbox.as_ref(),
        }
    }

    pub(crate) fn handle(&self) -> pc::SandboxHandle {
        self.sandbox().handle()
    }

    pub(crate) async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        self.sandbox().status().await
    }

    pub(crate) async fn dispose(&self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::dispose(sandbox.as_ref()).await,
            Self::Namespace { sandbox, hand } => {
                hand.stop().await;
                pc::Sandbox::dispose(sandbox.as_ref()).await
            }
            Self::Container { sandbox, hand, .. } => {
                hand.stop().await;
                sandbox.dispose().await
            }
        }
    }

    pub(crate) async fn quiesce(&self) {
        self.stop_bound_processes().await;
    }

    pub(crate) async fn checkpoint(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        self.sandbox().checkpoint(request, store).await
    }

    #[must_use]
    pub(crate) fn workdir(sandbox: LocalSandbox) -> Self {
        Self::Workdir(Arc::new(sandbox))
    }

    #[must_use]
    pub(crate) fn namespace(
        sandbox: NamespaceSandbox,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        hand_idle_after: std::time::Duration,
    ) -> Self {
        let sandbox = Arc::new(sandbox);
        let hand = Arc::new(SessionHandExecutor::namespace(
            sandbox.clone(),
            hand_factory,
            hand_bin,
            hand_idle_after,
        ));
        Self::Namespace { sandbox, hand }
    }

    async fn container(
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: &str,
        hand_idle_after: std::time::Duration,
        hand_residency: crate::deployment_config::ContainerHandResidency,
        capabilities: pc::SandboxCapabilities,
    ) -> Result<Self, pc::SandboxError> {
        let skills = Arc::new(ContainerSkillCache::default());
        let hand = Arc::new(
            SessionHandExecutor::container(
                sandbox.clone(),
                skills.clone(),
                hand_factory,
                hand_bin,
                hand_idle_after,
                hand_residency,
            )
            .await?,
        );
        Ok(Self::Container {
            sandbox,
            hand,
            skills,
            capabilities,
        })
    }

    /// Exact capability evidence of the provider that created this live
    /// environment. Resource hot-plug admission must inspect the resident
    /// environment rather than a process default that may select another tier.
    pub(crate) fn capabilities(&self) -> pc::SandboxCapabilities {
        match self {
            Self::Workdir(_) => awaken_sandbox_local::LocalProvider::capabilities(),
            Self::Namespace { .. } => awaken_sandbox_local::NamespaceProvider::capabilities(),
            Self::Container { capabilities, .. } => capabilities.clone(),
        }
    }

    /// Validate a complete replacement manifest against the resident backend's
    /// mount guarantees and hot-plug support before any projection is changed.
    pub(crate) fn validate_live_mount_replacement(
        &self,
        previous: &[pc::MountRequirement],
        next: &[pc::MountRequirement],
    ) -> Result<(), pc::SandboxError> {
        pc::validate_mount_requirements(next, &self.capabilities())
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        if previous != next
            && let Self::Container { sandbox, .. } = self
            && !sandbox.supports_live_mount_replacement(previous, next)
        {
            return Err(pc::SandboxError::new(
                "late mount replacement is unsupported for this container input set",
            ));
        }
        Ok(())
    }

    /// Fence tool dispatch and hibernate the current sandbox-resident Hand before
    /// changing the live resource projection. Workdir has no process-level path
    /// fidelity and retains its existing host-side structured-tool behavior.
    pub(crate) async fn begin_live_projection_update(
        &self,
    ) -> Result<Option<HandProjectionUpdate<'_>>, pc::SandboxError> {
        match self {
            Self::Namespace { hand, .. } | Self::Container { hand, .. } => {
                hand.begin_projection_update().await.map(Some)
            }
            Self::Workdir(_) => Ok(None),
        }
    }

    /// Executable Hand for this realized environment. Namespace and Container
    /// execute through one sandbox-resident channel; only the explicitly weaker
    /// Workdir tier uses host-side rooted tools.
    pub(crate) fn tool_executor(&self) -> Arc<dyn ToolExecutor> {
        match self {
            Self::Namespace { hand, .. } | Self::Container { hand, .. } => hand.clone(),
            Self::Workdir(_) => Arc::new(RawToolRegistry::new(self.rooted_tools())),
        }
    }

    pub(crate) fn rooted_tools(&self) -> Vec<Arc<dyn RawTool>> {
        match self {
            Self::Workdir(sandbox) => sandbox.rooted_tools(),
            // Descriptors remain the canonical built-in set; execution is forced
            // through this environment's bound Hand in `SessionCtx`.
            Self::Namespace { .. } | Self::Container { .. } => {
                awaken_ext_builtin_tools::all_hand_tools()
            }
        }
    }

    /// Stop only the process bindings created while constructing this wrapper.
    /// Used when an adoption races a resident environment with the same handle;
    /// disposing here would incorrectly destroy the shared underlying container.
    pub(crate) async fn stop_bound_processes(&self) {
        match self {
            Self::Namespace { hand, .. } | Self::Container { hand, .. } => hand.stop().await,
            Self::Workdir(_) => {}
        }
    }

    pub(crate) async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<(Box<dyn pc::ProcessHandle>, Box<dyn AgentChannelType>), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.spawn_agent(command).await,
            Self::Namespace { hand, .. } | Self::Container { hand, .. } => {
                hand.launcher().spawn_agent(command).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_provisioning_contract::{
        IsolationClass, NetworkPolicy, ResourceLimits, Sandbox, SandboxProvider, SandboxSpec,
    };
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_sandbox_local::{LocalProvider, NamespaceProvider};
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncReadExt;

    #[derive(Default)]
    struct FakeContainerProvider {
        creates: std::sync::atomic::AtomicUsize,
        specs: std::sync::Mutex<Vec<pc::SandboxSpec>>,
        renews: Arc<std::sync::atomic::AtomicUsize>,
        hand_spawns: Arc<std::sync::atomic::AtomicUsize>,
        resident_channel_opens: Arc<std::sync::atomic::AtomicUsize>,
        fail_hand_spawn_at: Arc<std::sync::atomic::AtomicUsize>,
        shared: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    }

    struct FakeContainer {
        renews: Arc<std::sync::atomic::AtomicUsize>,
        hand_spawns: Arc<std::sync::atomic::AtomicUsize>,
        resident_channel_opens: Arc<std::sync::atomic::AtomicUsize>,
        fail_hand_spawn_at: Arc<std::sync::atomic::AtomicUsize>,
        shared: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    }

    struct DoneProcess {
        id: String,
        code: i32,
    }

    struct UnreapableProcess;

    struct SlowReapProcess;

    impl DoneProcess {
        fn success(id: impl Into<String>) -> Self {
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

    struct FakeHandExecutor;

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
        ) -> Result<
            awaken_runtime_contract::tool::ToolOutput,
            awaken_runtime_contract::tool::ToolError,
        > {
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
            Err(pc::SandboxError::new("scripted signal failure"))
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
        async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
            Ok(())
        }

        async fn create_environment(
            &self,
            spec: &pc::SandboxSpec,
        ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError>
        {
            self.creates
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.specs.lock().unwrap().push(spec.clone());
            Ok(Arc::new(FakeContainer {
                renews: self.renews.clone(),
                hand_spawns: self.hand_spawns.clone(),
                resident_channel_opens: self.resident_channel_opens.clone(),
                fail_hand_spawn_at: self.fail_hand_spawn_at.clone(),
                shared: self.shared.clone(),
            }))
        }

        async fn adopt_environment(
            &self,
            _handle: &pc::SandboxHandle,
        ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError>
        {
            Ok(Arc::new(FakeContainer {
                renews: self.renews.clone(),
                hand_spawns: self.hand_spawns.clone(),
                resident_channel_opens: self.resident_channel_opens.clone(),
                fail_hand_spawn_at: self.fail_hand_spawn_at.clone(),
                shared: self.shared.clone(),
            }))
        }
    }

    #[async_trait]
    impl awaken_sandbox_container::ContainerEnvironment for FakeContainer {
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
        ) -> Result<awaken_sandbox_container::RuntimeAgentProcess, pc::SandboxError> {
            let (ours, theirs) = tokio::io::duplex(64 * 1024);
            let repository_export = command.argv.iter().any(|part| part == "bundle");
            if command.argv.iter().any(|part| part == "--stdio") {
                let spawn = self
                    .hand_spawns
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                if self
                    .fail_hand_spawn_at
                    .load(std::sync::atomic::Ordering::SeqCst)
                    == spawn
                {
                    return Err(pc::SandboxError::new("scripted Hand spawn failure"));
                }
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut theirs = theirs;
                    let mut sink = Vec::new();
                    let _ = theirs.read_to_end(&mut sink).await;
                });
            } else if command.argv.iter().any(|part| part.contains("cat")) {
                let bytes = self
                    .shared
                    .lock()
                    .unwrap()
                    .get("marker")
                    .cloned()
                    .unwrap_or_default();
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let mut theirs = theirs;
                    let _ = theirs.write_all(&bytes).await;
                });
            } else if repository_export {
                drop(theirs);
            } else {
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut theirs = theirs;
                    let mut bytes = Vec::new();
                    let _ = theirs.read_to_end(&mut bytes).await;
                });
            }
            let exit_code = if repository_export
                && self
                    .shared
                    .lock()
                    .unwrap()
                    .contains_key("__fail_repository_export")
            {
                19
            } else {
                0
            };
            let unreapable = command.argv.iter().any(|part| part == "--stdio")
                && self
                    .shared
                    .lock()
                    .unwrap()
                    .contains_key("__unreapable_hand");
            let slow_reap = command.argv.iter().any(|part| part == "--stdio")
                && self.shared.lock().unwrap().contains_key("__slow_reap_hand");
            let process: Box<dyn pc::ProcessHandle> = if unreapable {
                Box::new(UnreapableProcess)
            } else if slow_reap {
                Box::new(SlowReapProcess)
            } else {
                Box::new(DoneProcess::exited("container-exec", exit_code))
            };
            Ok(awaken_sandbox_container::RuntimeAgentProcess {
                process,
                channel: Box::new(ours),
            })
        }

        async fn read_files(
            &self,
            _root: &str,
        ) -> Result<Vec<awaken_sandbox_container::EnvironmentFile>, pc::SandboxError> {
            Ok(self
                .shared
                .lock()
                .unwrap()
                .iter()
                .map(|(path, bytes)| awaken_sandbox_container::EnvironmentFile {
                    path: path.clone(),
                    bytes: bytes.clone(),
                })
                .collect())
        }
    }

    #[async_trait]
    impl pc::Sandbox for FakeContainer {
        fn id(&self) -> &str {
            "session-container"
        }

        fn handle(&self) -> pc::SandboxHandle {
            pc::SandboxHandle::new("container", self.id())
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
            let repository_import = command.argv.iter().any(|part| part == "awaken-repo-import");
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
            _requirement: pc::MountRequirement,
        ) -> Result<pc::RealizedMount, pc::SandboxError> {
            Err(pc::SandboxError::new("unsupported"))
        }

        async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
            Ok(Vec::new())
        }

        async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, pc::SandboxError> {
            Err(pc::SandboxError::new("missing"))
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

        async fn dispose(&self) -> Result<(), pc::SandboxError> {
            Ok(())
        }
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
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
            lease_ttl_secs: None,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
        }
    }

    #[tokio::test]
    async fn native_process_and_agent_channel_share_one_live_environment() {
        // Cause/effect graph: C1=Workdir environment; C2=Native Hand tool;
        // C3=ACP child process. Effects: E1/E2 both observe state written inside
        // the same Session owner. Decision rule W1=C1+C2+C3 -> one marker value;
        // a host-global or provider-selected Hand would fail E1.
        let base = tempfile::tempdir().unwrap();
        let local = LocalProvider::new(base.path())
            .create_sandbox(&spec())
            .await
            .unwrap();
        let original = Sandbox::handle(&local);
        let environment = SessionEnvironment::workdir(local);
        assert_eq!(environment.handle(), original);

        let native = environment
            .sandbox()
            .spawn(pc::Command::new([
                "/bin/sh",
                "-c",
                "printf shared-state > marker",
            ]))
            .await
            .unwrap();
        assert_eq!(native.wait().await.unwrap().code, Some(0));

        let hand_output = environment
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "workdir-hand".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": "marker"}),
            })
            .await
            .unwrap();
        assert!(
            hand_output.text().contains("shared-state"),
            "W1 Native Hand"
        );

        let (agent, mut channel) = environment
            .spawn_agent(pc::Command::new(["/bin/sh", "-c", "cat marker"]))
            .await
            .unwrap();
        let mut output = String::new();
        channel.read_to_string(&mut output).await.unwrap();
        assert_eq!(agent.wait().await.unwrap().code, Some(0));
        assert_eq!(output, "shared-state");

        environment.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn namespace_native_process_and_agent_channel_share_one_live_environment() {
        // Decision rule N1 mirrors W1 at the Namespace tier: opaque ACP paths and
        // cooperative Native Hand calls must converge on one transparent workspace.
        let base = tempfile::tempdir().unwrap();
        let mut namespace_spec = spec();
        namespace_spec.scope = "session-namespace".into();
        namespace_spec.isolation = IsolationClass::Namespace;
        let provider = NamespaceProvider::new(base.path());
        if let Err(probe_error) = provider.probe_ready().await {
            let rejection = crate::sandbox_source::resolve_sandbox_tier(
                crate::deployment_config::SandboxTier::Namespace,
                false,
                base.path(),
            )
            .await
            .expect_err("an unavailable namespace must not silently degrade");
            assert!(rejection.contains("OS-native sandbox unavailable"));
            assert!(rejection.contains(&probe_error.to_string()));
            return;
        }
        let namespace = provider.create_sandbox(&namespace_spec).await.unwrap();
        let environment = SessionEnvironment::namespace(
            namespace,
            Arc::new(FakeHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
        #[cfg(target_os = "macos")]
        assert_eq!(environment.handle().provider_kind(), "seatbelt");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(environment.handle().provider_kind(), "bwrap");

        let mut native_command =
            pc::Command::new(["/bin/sh", "-c", "printf namespace-state > marker"]);
        native_command.cwd = "/workspace".into();
        let native = environment.sandbox().spawn(native_command).await.unwrap();
        assert_eq!(native.wait().await.unwrap().code, Some(0));

        let hand_output = environment
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "namespace-hand".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": "marker"}),
            })
            .await
            .unwrap();
        assert_eq!(hand_output.text(), "bound-hand-ok", "N1 Hand binding");

        let mut agent_command = pc::Command::new(["/bin/sh", "-c", "cat marker"]);
        agent_command.cwd = "/workspace".into();
        let (agent, mut channel) = environment.spawn_agent(agent_command).await.unwrap();
        let mut output = String::new();
        channel.read_to_string(&mut output).await.unwrap();
        assert_eq!(agent.wait().await.unwrap().code, Some(0));
        assert_eq!(output, "namespace-state");

        environment.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn container_native_tools_and_acp_share_one_environment_and_bound_hand() {
        let provider = Arc::new(FakeContainerProvider::default());
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        assert_eq!(
            provider.creates.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(environment.handle().provider_kind(), "container");
        assert!(AgentSandbox::is_container(&environment));
        // Config-home placement decision table:
        // C1=environment already exists; C2=root may be read-only; C3=workspace
        // is the provider's writable Session boundary. C1+C2+C3 requires both
        // the exposed and materialization paths to stay under /workspace.
        assert_eq!(
            AgentSandbox::config_home(&environment),
            "/workspace/.acp-config"
        );
        assert_eq!(
            AgentSandbox::config_home_logical(&environment),
            "/workspace/.acp-config"
        );

        AgentSandbox::materialize_inline(&environment, "/workspace/direct.bin", b"direct")
            .await
            .unwrap();
        environment
            .write_workspace_file("projected.bin", b"projected")
            .await
            .unwrap();
        environment
            .materialize_read_only_tree(
                "generated-skills",
                &[("skill/SKILL.md".into(), b"generated".to_vec(), false)],
            )
            .await
            .unwrap();
        environment
            .remove_projection_path("projected.bin")
            .await
            .unwrap();

        let native = environment
            .sandbox()
            .spawn(pc::Command::new(["sh", "-c", "write marker"]))
            .await
            .unwrap();
        assert_eq!(native.wait().await.unwrap().code, Some(0));
        let (agent, mut channel) = environment
            .spawn_agent(pc::Command::new(["sh", "-c", "cat marker"]))
            .await
            .unwrap();
        let mut output = String::new();
        channel.read_to_string(&mut output).await.unwrap();
        assert_eq!(agent.wait().await.unwrap().code, Some(0));
        assert_eq!(output, "shared-container-state");

        let hand = environment.tool_executor();
        environment.register_skill_dir("skills");
        provider.shared.lock().unwrap().insert(
            "authored/SKILL.md".into(),
            b"---\ndescription: authored\n---\nbody".to_vec(),
        );
        let result = hand
            .invoke(&ToolCall {
                call_id: "bound-hand".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({"command": "printf bound-hand-ok"}),
            })
            .await
            .unwrap();
        assert!(result.text().contains("bound-hand-ok"));
        let skills = environment.scan_skill_dir("skills");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "authored");
        assert_eq!(skills[0].dir, "skills/authored");
        environment.refresh_skills().await.unwrap();
        environment.stop_bound_processes().await;
        environment.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn prepared_image_is_the_only_environment_to_container_image_conversion() {
        // FMECA: F1 every authored Environment is implicitly converted to an
        // image (S6/O5/D4, RPN120) -> self-hosted/package-free paths become
        // dependent on a builder; F2 a ready immutable image is omitted from the
        // provider request (S9/O3/D5, RPN135) -> packages are resolved again at
        // runtime; F3 both prepared image and mutable packages reach the provider
        // (S8/O3/D4, RPN96) -> two realization tracks can diverge. Mitigation is
        // the canonical Snapshot projection: `prepared_image=Some` selects one
        // Image environment and clears packages; `None` retains package inputs.
        //
        // Cause/effect decision table:
        // | Rule | prepared image | packages | final provider SandboxSpec |
        // | I1 | none | non-empty | no image; exact packages retained |
        // | I2 | ready digest | non-empty | Image(digest); packages empty |
        // This test crosses the final `SessionEnvironmentProvider::create`
        // boundary, so it verifies the actual container adapter input rather
        // than only an intermediate projection helper.
        fn snapshot(
            prepared_image: Option<String>,
        ) -> awaken_session_contract::EnvironmentSnapshot {
            awaken_session_contract::EnvironmentSnapshot {
                environment_id: "image-flow".into(),
                revision: awaken_session_contract::EnvironmentRevision(7),
                self_hosted: false,
                config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                    "image-flow-v7".into(),
                ),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: awaken_session_contract::EnvironmentPackages {
                    npm: vec!["tsx@4".into()],
                    ..Default::default()
                },
                prepared_image,
                network: awaken_session_contract::SessionNetworkPolicy::None,
                credential_realization:
                    awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
            }
        }

        let provider = Arc::new(FakeContainerProvider::default());
        let environments = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        );
        let unprepared =
            crate::provisioning::environment_capacity_projection(&snapshot(None), true).spec;
        let environment = environments.create(&unprepared).await.unwrap();
        environment.dispose().await.unwrap();

        let digest = "registry.example/awaken@sha256:0123456789abcdef";
        let prepared = crate::provisioning::environment_capacity_projection(
            &snapshot(Some(digest.into())),
            true,
        )
        .spec;
        let environment = environments.create(&prepared).await.unwrap();
        environment.dispose().await.unwrap();

        let specs = provider.specs.lock().unwrap();
        assert_eq!(specs.len(), 2, "I1/I2 provider boundary");
        assert_eq!(
            specs[0].packages.managers.get("npm"),
            Some(&vec!["tsx@4".to_string()]),
            "I1"
        );
        assert!(specs[0].environment.is_none(), "I1");
        assert!(specs[1].packages.managers.is_empty(), "I2/F3");
        assert_eq!(
            specs[1].environment,
            Some(awaken_provisioning_contract::EnvironmentKind::Image {
                reference: digest.into()
            }),
            "I2"
        );
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
        ) -> Result<
            awaken_runtime_contract::tool::ToolOutput,
            awaken_runtime_contract::tool::ToolError,
        > {
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
                    Err(awaken_runtime_contract::tool::ToolError::Execution(
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

    struct BlockingHandFactory {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        block_once: Arc<std::sync::atomic::AtomicBool>,
    }

    struct BlockingHandExecutor {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        block_once: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl ToolExecutor for BlockingHandExecutor {
        async fn invoke(
            &self,
            call: &ToolCall,
        ) -> Result<
            awaken_runtime_contract::tool::ToolOutput,
            awaken_runtime_contract::tool::ToolError,
        > {
            if self.block_once.swap(false, Ordering::SeqCst) {
                self.started.notify_one();
                self.release.notified().await;
            }
            Ok(awaken_runtime_contract::tool::ToolOutput::ok(
                &call.call_id,
                "completed",
            ))
        }
    }

    impl HandExecutorFactory for BlockingHandFactory {
        fn bind(
            &self,
            _channel: Box<dyn AgentChannelType>,
            _operation_scope: &str,
            _recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
        ) -> Arc<dyn ToolExecutor> {
            Arc::new(BlockingHandExecutor {
                started: self.started.clone(),
                release: self.release.clone(),
                block_once: self.block_once.clone(),
            })
        }
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

    #[tokio::test]
    async fn resident_hand_survives_worker_detach_and_is_reopened_on_adoption() {
        /*
         * Resident-Hand HA cause/effect graph and decision table.
         * Causes: C1 residency=resident; C2 Session Pod exists; C3 original
         * Worker attachment closes; C4 replacement Worker adopts the handle;
         * C5 Pod/Hand itself fails. Constraints: C4 requires C2; C5 excludes
         * healthy adoption. Effects: E1 no attached `hand --stdio` child; E2
         * open one provider-owned Pod channel per Worker binding; E3 advertise
         * DurableRequest recovery; E4 detaching does not signal the resident
         * process; E5 Pod/Hand failure is not masked as Worker recovery.
         * Rules: RH1 C1+C2=>E1+E2+E3; RH2 C1+C2+C3+C4=>E2+E4;
         * RH3 C5=>E5 (covered by channel/open failure tests).
         * FMECA: Worker crash loses only the ephemeral channel (severity 2,
         * detectable by channel failure); re-adoption/open-channel is the
         * mitigation. A Pod/Hand crash remains severity 4 and requires workload
         * recovery, not a duplicate Worker-side Hand.
         */
        let provider = Arc::new(FakeContainerProvider::default());
        let factory =
            ScriptedHandFactory::new([ScriptedHandOutcome::Success, ScriptedHandOutcome::Success]);
        let environments =
            SessionEnvironmentProvider::container_with_capacity_hand_idle_and_residency(
                provider.clone(),
                None,
                Vec::new(),
                factory,
                "/usr/local/bin/awaken-sandbox",
                std::time::Duration::ZERO,
                crate::deployment_config::ContainerHandResidency::Resident,
            );

        let original = environments.create(&spec()).await.unwrap();
        let handle = original.handle();
        assert_eq!(
            original.tool_executor().recovery_capability("write"),
            awaken_runtime_contract::tool::ToolRecoveryCapability::DurableRequest
        );
        assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 0);
        assert_eq!(provider.resident_channel_opens.load(Ordering::SeqCst), 1);

        original.stop_bound_processes().await;
        let adopted = environments.adopt(&handle).await.unwrap();
        assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 0);
        assert_eq!(provider.resident_channel_opens.load(Ordering::SeqCst), 2);
        assert_eq!(provider.renews.load(Ordering::SeqCst), 1);
        assert_eq!(
            adopted.tool_executor().recovery_capability("bash"),
            awaken_runtime_contract::tool::ToolRecoveryCapability::DurableRequest
        );
    }

    #[tokio::test]
    async fn container_hand_reacquires_only_for_a_proven_pre_dispatch_failure() {
        /*
         * Container-Hand recovery cause/effect decision table.
         * Causes: C1 first binding succeeds or is unavailable before dispatch;
         * C2 one or two calls arrive; C3 replacement succeeds or is also
         * unavailable; C4 failure occurs after dispatch (indeterminate); C5 the
         * replacement process cannot start; C6 the owner is already closed.
         * Effects: E1 use the resident Hand without spawning; E2 stop the expired
         * binding, spawn exactly one replacement, and safely retry; E3 serialize
         * concurrent recovery behind that one replacement; E4 return after one
         * bounded retry; E5 never replay an indeterminate call; E6 propagate a
         * replacement-start failure without a loop; E7 never restart after close;
         * E8 idle hibernation and its stale-timer races are covered separately.
         * Rules: H1 success=>E1; H2 unavailable+C2+C3(success)=>E2+E3;
         * H3 unavailable+C3(unavailable)=>E4; H4 C4=>E5; H5 C5=>E6;
         * H6 C6=>E7.
         */
        let provider = Arc::new(FakeContainerProvider::default());
        let stable = ScriptedHandFactory::new([ScriptedHandOutcome::Success]);
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            stable.clone(),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        let hand = environment.tool_executor();
        tokio::task::yield_now().await;
        assert_eq!(
            hand.invoke(&ToolCall {
                call_id: "stable".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap()
            .text(),
            "recovered"
        );
        assert_eq!(stable.binds.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            provider
                .hand_spawns
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        environment.stop_bound_processes().await;
        assert!(matches!(
            hand.invoke(&ToolCall {
                call_id: "closed".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await,
            Err(awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_))
        ));
        assert_eq!(
            provider
                .hand_spawns
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a closed owner never launches another Hand"
        );
        environment.dispose().await.unwrap();

        let provider = Arc::new(FakeContainerProvider::default());
        let recover = ScriptedHandFactory::new([
            ScriptedHandOutcome::UnavailableBeforeDispatch,
            ScriptedHandOutcome::Success,
        ]);
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            recover.clone(),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        let hand = environment.tool_executor();
        let left = ToolCall {
            call_id: "left".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        };
        let right = ToolCall {
            call_id: "right".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        };
        let (left, right) = tokio::join!(hand.invoke(&left), hand.invoke(&right));
        assert_eq!(left.unwrap().text(), "recovered");
        assert_eq!(right.unwrap().text(), "recovered");
        assert_eq!(recover.binds.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            provider
                .hand_spawns
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "concurrent callers share one replacement"
        );
        environment.dispose().await.unwrap();

        let provider = Arc::new(FakeContainerProvider::default());
        provider
            .fail_hand_spawn_at
            .store(2, std::sync::atomic::Ordering::SeqCst);
        let failed_replacement =
            ScriptedHandFactory::new([ScriptedHandOutcome::UnavailableBeforeDispatch]);
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            failed_replacement.clone(),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        let error = environment
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "replacement-spawn-failure".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .expect_err("a replacement spawn failure is propagated");
        assert!(matches!(
            error,
            awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(ref message)
                if message.contains("failed to reacquire Session hand")
        ));
        assert_eq!(
            provider
                .hand_spawns
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one initial spawn plus one failed replacement attempt"
        );
        assert_eq!(
            failed_replacement
                .binds
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a failed process spawn never creates an executor"
        );
        environment.dispose().await.unwrap();

        let provider = Arc::new(FakeContainerProvider::default());
        let bounded = ScriptedHandFactory::new([
            ScriptedHandOutcome::UnavailableBeforeDispatch,
            ScriptedHandOutcome::UnavailableBeforeDispatch,
            ScriptedHandOutcome::Success,
        ]);
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            bounded.clone(),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        let error = environment
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "bounded".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .expect_err("a second dead channel ends the bounded retry");
        assert!(matches!(
            error,
            awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_)
        ));
        assert_eq!(bounded.binds.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            provider
                .hand_spawns
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        environment.dispose().await.unwrap();

        let provider = Arc::new(FakeContainerProvider::default());
        let indeterminate = ScriptedHandFactory::new([ScriptedHandOutcome::Indeterminate]);
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            indeterminate.clone(),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        let error = environment
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "indeterminate".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .expect_err("a possibly executed call is never replayed");
        assert!(matches!(
            error,
            awaken_runtime_contract::tool::ToolError::Execution(_)
        ));
        assert_eq!(
            indeterminate
                .binds
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            provider
                .hand_spawns
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        environment.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn live_projection_update_hibernates_and_fences_the_session_hand() {
        /*
         * Projection/Hand cause-effect graph and decision table.
         * Causes: C1=a bound Hand exists; C2=projection update begins; C3=the
         * update commits; C4=the update guard drops without commit; C5=a tool
         * arrives during/after the update. Effects: E1=wait for the in-flight
         * binding and reap it once; E2=reject C5 before dispatch while fenced;
         * E3=after C3 lazily launch exactly one Hand over the new projection;
         * E4=after C4 remain fenced until the authoritative retry commits.
         * Rules: U1 C1+C2=>E1+E2; U2 U1+C3+C5=>E3;
         * U3 U1+C4+C5=>E2+E4.
         */
        let provider = Arc::new(FakeContainerProvider::default());
        let factory = ScriptedHandFactory::new([
            ScriptedHandOutcome::Success,
            ScriptedHandOutcome::Success,
            ScriptedHandOutcome::Success,
        ]);
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            factory,
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        let hand = environment.tool_executor();

        let committed = environment
            .begin_live_projection_update()
            .await
            .unwrap()
            .expect("container Hand update");
        assert!(
            matches!(
                hand.invoke(&ToolCall {
                    call_id: "during-update".into(),
                    tool_id: "read".into(),
                    arguments: serde_json::json!({}),
                })
                .await,
                Err(awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_))
            ),
            "U1/E2"
        );
        committed.commit();
        hand.invoke(&ToolCall {
            call_id: "after-commit".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
        assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 2, "U2/E3");

        let uncommitted = environment
            .begin_live_projection_update()
            .await
            .unwrap()
            .expect("container Hand update");
        drop(uncommitted);
        assert!(
            matches!(
                hand.invoke(&ToolCall {
                    call_id: "after-failed-update".into(),
                    tool_id: "read".into(),
                    arguments: serde_json::json!({}),
                })
                .await,
                Err(awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_))
            ),
            "U3/E2,E4"
        );
        assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 2, "U3/E4");

        environment
            .begin_live_projection_update()
            .await
            .unwrap()
            .expect("retry update")
            .commit();
        hand.invoke(&ToolCall {
            call_id: "after-retry".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
        assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 3, "U3 retry");
        environment.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn live_projection_update_rejects_an_unreapable_hand() {
        // A failed reap leaves the old process outcome unknown. An empty local
        // binding is therefore not evidence that the projection can proceed:
        // the owner must remain closed and must never launch a second Hand.
        let provider = Arc::new(FakeContainerProvider::default());
        provider
            .shared
            .lock()
            .unwrap()
            .insert("__unreapable_hand".into(), Vec::new());
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        let hand = environment.tool_executor();

        let error = match environment.begin_live_projection_update().await {
            Err(error) => error,
            Ok(_) => panic!("an unknown old-process outcome must reject the projection update"),
        };
        assert!(
            error
                .to_string()
                .contains("failed to reap Session hand before projection update")
        );
        assert!(matches!(
            hand.invoke(&ToolCall {
                call_id: "after-projection-reap-failure".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await,
            Err(awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_))
        ));
        assert_eq!(
            provider.hand_spawns.load(Ordering::SeqCst),
            1,
            "a failed projection reap never permits a replacement Hand"
        );
        environment.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_projection_reap_retains_the_tracked_hand_owner() {
        let provider = Arc::new(FakeContainerProvider::default());
        provider
            .shared
            .lock()
            .unwrap()
            .insert("__slow_reap_hand".into(), Vec::new());
        let environment = Arc::new(
            SessionEnvironmentProvider::container(
                provider.clone(),
                Vec::new(),
                ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
                "/usr/local/bin/awaken-sandbox",
            )
            .create(&spec())
            .await
            .unwrap(),
        );
        let updating = tokio::spawn({
            let environment = environment.clone();
            async move {
                let update = environment
                    .begin_live_projection_update()
                    .await?
                    .expect("Container projection has a Hand fence");
                update.commit();
                Ok::<_, pc::SandboxError>(())
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        updating.abort();
        let _ = updating.await;

        let SessionEnvironment::Container { hand, .. } = environment.as_ref() else {
            panic!("test uses a Container environment")
        };
        assert!(
            hand.has_tracked_binding().await,
            "cancellation must retain the only known process owner"
        );
        assert!(hand.projection_is_updating());
        assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1);
    }

    /// Worker-local Hand inactivity cause/effect decision table.
    /// C1=idle policy enabled; C2=deadline reached; C3=a newer invocation touches
    /// the generation; C4=policy is zero; C5=invocation follows hibernation.
    /// E1=keep one binding before the deadline; E2=stale deadline cannot stop a
    /// newer generation; E3=deadline releases the Hand; E4=next call lazily
    /// creates exactly one replacement; E5=zero disables hibernation; C6=the
    /// provider cannot reap the expired process; E6=close the owner and never
    /// launch a possibly concurrent replacement. Rules:
    /// I1 C1+!C2=>E1; I2 C1+C2+C3=>E2; I3 C1+C2+!C3=>E3;
    /// I4 I3+C5=>E4; I5 C4=>E5; I6 C6=>E6. This executes in the Runtime Host
    /// without a Coordinator or durable Session scan, covering split deployment
    /// ownership.
    #[tokio::test(start_paused = true)]
    async fn container_hand_hibernates_on_worker_local_inactivity_and_reacquires_once() {
        let provider = Arc::new(FakeContainerProvider::default());
        let factory =
            ScriptedHandFactory::new([ScriptedHandOutcome::Success, ScriptedHandOutcome::Success]);
        let environment = SessionEnvironmentProvider::container_with_capacity_and_hand_idle(
            provider.clone(),
            None,
            Vec::new(),
            factory.clone(),
            "/usr/local/bin/awaken-sandbox",
            std::time::Duration::from_secs(60),
        )
        .create(&spec())
        .await
        .unwrap();
        let hand = environment.tool_executor();

        tokio::time::advance(std::time::Duration::from_secs(59)).await;
        tokio::task::yield_now().await;
        assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1, "I1");

        hand.invoke(&ToolCall {
            call_id: "refresh-deadline".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1, "I2");

        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        hand.invoke(&ToolCall {
            call_id: "after-idle".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
        assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 2, "I3+I4");
        assert_eq!(factory.binds.load(Ordering::SeqCst), 2, "I4");
        environment.dispose().await.unwrap();

        let disabled_provider = Arc::new(FakeContainerProvider::default());
        let disabled = SessionEnvironmentProvider::container(
            disabled_provider.clone(),
            Vec::new(),
            ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(3_600)).await;
        tokio::task::yield_now().await;
        disabled
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "disabled".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap();
        assert_eq!(
            disabled_provider.hand_spawns.load(Ordering::SeqCst),
            1,
            "I5"
        );
        disabled.dispose().await.unwrap();

        let failed_provider = Arc::new(FakeContainerProvider::default());
        failed_provider
            .shared
            .lock()
            .unwrap()
            .insert("__unreapable_hand".into(), Vec::new());
        let failed = SessionEnvironmentProvider::container_with_capacity_and_hand_idle(
            failed_provider.clone(),
            None,
            Vec::new(),
            ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
            "/usr/local/bin/awaken-sandbox",
            std::time::Duration::from_secs(60),
        )
        .create(&spec())
        .await
        .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert!(matches!(
            failed
                .tool_executor()
                .invoke(&ToolCall {
                    call_id: "after-reap-failure".into(),
                    tool_id: "read".into(),
                    arguments: serde_json::json!({}),
                })
                .await,
            Err(awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_))
        ));
        assert_eq!(failed_provider.hand_spawns.load(Ordering::SeqCst), 1, "I6");
        failed.dispose().await.unwrap();
    }

    /// Idle/invoke race rule I7: C1=the old deadline fires while a Hand call owns
    /// the lifecycle mutex; C2=the call completes and advances its generation;
    /// E1=the waiting timer observes the new generation and cannot reap the live
    /// binding; E2=the next call reuses that same Hand without a second spawn.
    #[tokio::test(start_paused = true)]
    async fn an_idle_deadline_cannot_reap_a_concurrent_hand_invocation() {
        let provider = Arc::new(FakeContainerProvider::default());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let environment = SessionEnvironmentProvider::container_with_capacity_and_hand_idle(
            provider.clone(),
            None,
            Vec::new(),
            Arc::new(BlockingHandFactory {
                started: started.clone(),
                release: release.clone(),
                block_once: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            }),
            "/usr/local/bin/awaken-sandbox",
            std::time::Duration::from_secs(60),
        )
        .create(&spec())
        .await
        .unwrap();
        tokio::task::yield_now().await;
        let hand = environment.tool_executor();
        let running_hand = hand.clone();
        let running = tokio::spawn(async move {
            running_hand
                .invoke(&ToolCall {
                    call_id: "running-at-deadline".into(),
                    tool_id: "read".into(),
                    arguments: serde_json::json!({}),
                })
                .await
        });
        started.notified().await;
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        release.notify_one();
        assert_eq!(running.await.unwrap().unwrap().text(), "completed");
        tokio::task::yield_now().await;

        hand.invoke(&ToolCall {
            call_id: "reuse-after-race".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
        assert_eq!(
            provider.hand_spawns.load(Ordering::SeqCst),
            1,
            "I7: a stale timer never forces replacement"
        );
        environment.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn adopting_a_container_renews_its_ownership_before_use() {
        let provider = Arc::new(FakeContainerProvider::default());
        let environments = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        );
        let adopted = environments
            .adopt(&pc::SandboxHandle::new("container", "session-container"))
            .await
            .expect("adopt environment");
        assert_eq!(
            provider.renews.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "adoption refreshes ownership before starting the replacement hand"
        );
        adopted.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn container_repository_transfer_reports_import_and_export_failures() {
        let source_root = tempfile::tempdir().expect("source root");
        let source = source_root.path().join("source");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .status()
                .expect("run git fixture command");
            assert!(status.success(), "git fixture command failed: {args:?}");
        };
        git(&["init", "-q", source.to_str().unwrap()]);
        git(&[
            "-C",
            source.to_str().unwrap(),
            "config",
            "user.name",
            "fixture",
        ]);
        git(&[
            "-C",
            source.to_str().unwrap(),
            "config",
            "user.email",
            "fixture@example.invalid",
        ]);
        std::fs::write(source.join("README.md"), "fixture").unwrap();
        git(&["-C", source.to_str().unwrap(), "add", "README.md"]);
        git(&[
            "-C",
            source.to_str().unwrap(),
            "commit",
            "-q",
            "-m",
            "fixture",
        ]);

        let shared = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let container = FakeContainer {
            renews: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hand_spawns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            resident_channel_opens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            fail_hand_spawn_at: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shared: shared.clone(),
        };
        shared
            .lock()
            .unwrap()
            .insert("__fail_repository_import".into(), Vec::new());
        let import_error = container_repositories::provision(
            &container,
            "repo",
            source.to_str().unwrap(),
            None,
            None,
            None,
        )
        .await
        .expect_err("a failed container import is not reported as provisioned");
        assert!(
            import_error
                .to_string()
                .contains("container repository import exited Some(23)")
        );

        {
            let mut state = shared.lock().unwrap();
            state.remove("__fail_repository_import");
            state.insert("__fail_repository_export".into(), Vec::new());
        }
        let export_error =
            container_repositories::push(&container, "repo", source.to_str().unwrap(), None)
                .await
                .expect_err("a failed container export is not pushed");
        assert!(
            export_error
                .to_string()
                .contains("container repository export exited Some(19)")
        );
    }

    #[tokio::test]
    async fn provider_rebasing_adoption_and_container_mount_jail_cover_every_tier() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();

        let workdir = SessionEnvironmentProvider::workdir(first.path()).at_root(second.path());
        assert!(matches!(workdir, SessionEnvironmentProvider::Workdir(_)));

        let namespace = SessionEnvironmentProvider::namespace_with_agent_stderr(
            first.path(),
            true,
            Arc::new(FakeHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        )
        .at_root(second.path());
        assert!(matches!(
            &namespace,
            SessionEnvironmentProvider::Namespace { provider, .. }
                if provider.inherits_agent_stderr()
        ));
        let mut namespace_spec = spec();
        namespace_spec.scope = "provider-namespace-adopt".into();
        let created = namespace.create(&namespace_spec).await.unwrap();
        let handle = created.handle();
        drop(created);
        let adopted = namespace.adopt(&handle).await.unwrap();
        assert_eq!(adopted.handle(), handle);
        adopted.dispose().await.unwrap();

        let provider = Arc::new(FakeContainerProvider::default());
        let unsafe_mount = pc::MountRequirement {
            mount_id: "unsafe".into(),
            source: pc::MountSource::Inline {
                contents: "value".into(),
            },
            mount_path: "../escape".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        };
        let container = SessionEnvironmentProvider::container(
            provider,
            vec![unsafe_mount],
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        )
        .at_root(second.path());
        assert!(matches!(
            container,
            SessionEnvironmentProvider::Container { .. }
        ));
        assert!(container.create(&spec()).await.is_err());
    }

    #[cfg(feature = "container-docker")]
    #[tokio::test]
    async fn docker_environment_transfers_repo_and_harvests_files_without_exposing_token() {
        let Ok(image) = std::env::var("AWAKEN_TEST_SESSION_IMAGE") else {
            eprintln!("skipping: AWAKEN_TEST_SESSION_IMAGE is not set");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        let seed = temp.path().join("seed");
        let git = |cwd: &std::path::Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        git(
            temp.path(),
            &["clone", remote.to_str().unwrap(), seed.to_str().unwrap()],
        );
        git(&seed, &["config", "user.name", "seed"]);
        git(&seed, &["config", "user.email", "seed@example.invalid"]);
        std::fs::write(seed.join("README.md"), "base").unwrap();
        git(&seed, &["add", "README.md"]);
        git(&seed, &["commit", "-m", "base"]);
        git(&seed, &["push", "-u", "origin", "HEAD"]);

        let runtime =
            Arc::new(awaken_sandbox_container::docker::DockerRuntime::connect_local(8080).unwrap());
        let provider = Arc::new(awaken_sandbox_container::ContainerProvider::new(
            runtime, image,
        ));
        let mut docker_spec = spec();
        docker_spec.scope = format!("host-repo-real-{}", std::process::id());
        docker_spec.outputs_path = "/mnt/session/outputs".into();
        let environment = SessionEnvironmentProvider::container(
            provider,
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&docker_spec)
        .await
        .unwrap();
        let repository_plan = pc::RepositoryRealizationPlan {
            repository_id: "repo".into(),
            mount_path: "workspace/repo".into(),
            remote_url: remote.to_string_lossy().into_owned(),
            initial_branch: None,
            initial_commit: None,
            access: pc::MountAccess::ReadWrite,
        };
        pc::RepositoryRealizer::realize_repository(&environment, &repository_plan, None)
            .await
            .unwrap();
        environment
            .materialize_inline("/workspace/.mnt/live.txt", b"live")
            .await
            .unwrap();
        assert_eq!(
            environment.list_workspace_files(".mnt").await.unwrap(),
            vec![("live.txt".into(), b"live".to_vec())]
        );
        environment
            .remove_projection_path(".mnt/live.txt")
            .await
            .unwrap();
        assert!(
            environment
                .list_workspace_files(".mnt")
                .await
                .unwrap()
                .is_empty()
        );

        let change = environment
            .sandbox()
            .spawn(pc::Command::new([
                "sh",
                "-c",
                concat!(
                    "git -C /workspace/repo config user.name agent && ",
                    "git -C /workspace/repo config user.email agent@example.invalid && ",
                    "printf changed > /workspace/repo/README.md && ",
                    "git -C /workspace/repo add README.md && ",
                    "git -C /workspace/repo commit -m changed && ",
                    "mkdir -p /workspace/outputs/nested && ",
                    "printf '\\000\\377' > /workspace/outputs/nested/result.bin && ",
                    "mkdir -p /workspace/skills/authored && ",
                    "printf '%s' '---\ndescription: authored\n---\nbody' > ",
                    "/workspace/skills/authored/SKILL.md"
                ),
            ]))
            .await
            .unwrap();
        assert_eq!(change.wait().await.unwrap().code, Some(0));
        environment.register_skill_dir("skills");
        environment.refresh_skills().await.unwrap();
        let skills = environment.scan_skill_dir("skills");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "authored");
        assert!(
            pc::RepositoryRealizer::publish_repository(&environment, &repository_plan, None)
                .await
                .unwrap()
        );
        assert!(
            !pc::RepositoryRealizer::publish_repository(&environment, &repository_plan, None)
                .await
                .unwrap()
        );
        // Artifact cause/effect rule: one regular output file → one canonical
        // content-addressed Artifact and binary-safe read through the same port.
        let artifacts = environment.artifacts().await.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert!(artifacts[0].path.ends_with("nested/result.bin"));
        assert_eq!(artifacts[0].id, artifacts[0].content_hash);
        assert_eq!(
            environment.read_artifact(&artifacts[0].id).await.unwrap(),
            vec![0, 0xff]
        );
        environment.dispose().await.unwrap();

        let count = std::process::Command::new("git")
            .args([
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-list",
                "--count",
                "--all",
            ])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "2");
    }
}
