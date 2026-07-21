//! The single live execution environment owned by a Session.
//!
//! Host code depends on this capability object instead of retaining a concrete
//! sandbox in `SessionCtx`. Workdir is the first adapter; Namespace and Container
//! plug into the same owner without adding another Native/ACP lifecycle.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_run_executor_acp::AgentChannelType;
use awaken_runtime_contract::tool::RawTool;
use awaken_runtime_contract::tool::ToolExecutor;
use awaken_sandbox_local::{
    DiscoveredSkillFile, LocalProvider, LocalSandbox, NamespaceProvider, NamespaceSandbox,
};

/// Composition port that binds a live hand channel to the runtime's neutral tool
/// executor. The framing implementation belongs to an outer composition crate;
/// this host owns only the Session lifecycle and never imports the relay adapter.
pub trait HandExecutorFactory: Send + Sync {
    fn bind(
        &self,
        channel: Box<dyn AgentChannelType>,
        operation_scope: &str,
    ) -> Arc<dyn ToolExecutor>;
}

/// Creates/adopts the one Session environment while auxiliary housekeeping Runs
/// may continue using their deliberately-fresh LocalProvider.
pub(crate) enum SessionEnvironmentProvider {
    Workdir(LocalProvider),
    Namespace(NamespaceProvider),
    Container {
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
    },
}

impl SessionEnvironmentProvider {
    pub(crate) fn workdir(base: impl Into<std::path::PathBuf>) -> Self {
        Self::Workdir(LocalProvider::new(base))
    }

    pub(crate) fn namespace(base: impl Into<std::path::PathBuf>) -> Self {
        Self::Namespace(NamespaceProvider::new(base))
    }

    pub(crate) fn container(
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
    ) -> Self {
        Self::Container {
            provider,
            extra_mounts,
            hand_factory,
        }
    }

    pub(crate) fn at_root(&self, base: impl Into<std::path::PathBuf>) -> Self {
        let base = base.into();
        match self {
            Self::Workdir(_) => Self::workdir(base),
            Self::Namespace(_) => Self::namespace(base),
            Self::Container {
                provider,
                extra_mounts,
                hand_factory,
            } => Self::Container {
                provider: provider.clone(),
                extra_mounts: extra_mounts.clone(),
                hand_factory: hand_factory.clone(),
            },
        }
    }

    pub(crate) async fn create(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        match self {
            Self::Workdir(provider) => provider
                .create_sandbox(spec)
                .await
                .map(SessionEnvironment::workdir),
            Self::Namespace(provider) => {
                let mut spec = spec.clone();
                spec.isolation = pc::IsolationClass::Namespace;
                provider
                    .create_sandbox(&spec)
                    .await
                    .map(SessionEnvironment::namespace)
            }
            Self::Container {
                provider,
                extra_mounts,
                hand_factory,
            } => {
                let mut spec = spec.clone();
                spec.isolation = pc::IsolationClass::Container;
                spec.mounts.extend(extra_mounts.iter().cloned());
                let environment = provider.create_environment(&spec).await?;
                SessionEnvironment::container(environment, hand_factory.as_ref()).await
            }
        }
    }

    pub(crate) async fn adopt(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        match self {
            Self::Workdir(provider) => provider
                .adopt_sandbox(handle)
                .await
                .map(SessionEnvironment::workdir),
            Self::Namespace(provider) => provider
                .adopt_sandbox(handle)
                .await
                .map(SessionEnvironment::namespace),
            Self::Container {
                provider,
                hand_factory,
                ..
            } => {
                let environment = provider.adopt_environment(handle).await?;
                SessionEnvironment::container(environment, hand_factory.as_ref()).await
            }
        }
    }
}

/// Segregated capability needed by the ACP channel adapter. Keeping it beside
/// the Session owner avoids teaching the neutral provisioning contract about
/// async byte channels.
#[async_trait]
pub(crate) trait AgentSandbox: Send + Sync {
    fn is_container(&self) -> bool;

    fn config_home(&self) -> &'static str;

    async fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError>;

    async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<
        (
            Box<dyn pc::ProcessHandle>,
            Box<dyn awaken_run_executor_acp::AgentChannelType>,
        ),
        pc::SandboxError,
    >;
}

#[async_trait]
impl AgentSandbox for LocalSandbox {
    fn is_container(&self) -> bool {
        false
    }

    fn config_home(&self) -> &'static str {
        ".acp-config"
    }

    async fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        self.materialize_inline(logical, contents)
    }

    async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<
        (
            Box<dyn pc::ProcessHandle>,
            Box<dyn awaken_run_executor_acp::AgentChannelType>,
        ),
        pc::SandboxError,
    > {
        self.spawn_agent(command).await
    }
}

/// One realized sandbox shared by every Run attempt in a Session.
pub(crate) enum SessionEnvironment {
    Workdir(Arc<LocalSandbox>),
    Namespace(Arc<NamespaceSandbox>),
    Container {
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        hand_process: Arc<dyn pc::ProcessHandle>,
        hand: Arc<dyn ToolExecutor>,
    },
}

impl SessionEnvironment {
    #[must_use]
    pub(crate) fn workdir(sandbox: LocalSandbox) -> Self {
        Self::Workdir(Arc::new(sandbox))
    }

    #[must_use]
    pub(crate) fn namespace(sandbox: NamespaceSandbox) -> Self {
        Self::Namespace(Arc::new(sandbox))
    }

    async fn container(
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        hand_factory: &dyn HandExecutorFactory,
    ) -> Result<Self, pc::SandboxError> {
        let hand_bin = std::env::var("AWAKEN_CONTAINER_HAND_BIN")
            .unwrap_or_else(|_| "/usr/local/bin/awaken-sandbox".to_string());
        let process = sandbox
            .spawn_agent_process(pc::Command {
                argv: vec![hand_bin, "hand".into(), "--stdio".into()],
                cwd: "/workspace".into(),
                env: Vec::new(),
                stdio: pc::Stdio::Piped,
            })
            .await?;
        let hand = hand_factory.bind(process.channel, sandbox.id());
        Ok(Self::Container {
            sandbox,
            hand_process: Arc::from(process.process),
            hand,
        })
    }

    pub(crate) fn bound_tool_executor(&self) -> Option<Arc<dyn ToolExecutor>> {
        match self {
            Self::Container { hand, .. } => Some(hand.clone()),
            Self::Workdir(_) | Self::Namespace(_) => None,
        }
    }

    pub(crate) fn rooted_tools(&self) -> Vec<Arc<dyn RawTool>> {
        match self {
            Self::Workdir(sandbox) => sandbox.rooted_tools(),
            Self::Namespace(sandbox) => sandbox.rooted_tools(),
            // Descriptors remain the canonical built-in set; execution is forced
            // through this environment's bound remote hand in `SessionCtx`.
            Self::Container { .. } => awaken_ext_builtin_tools::executable_hand_tools(),
        }
    }

    pub(crate) fn provision_repo(
        &self,
        logical: &str,
        url: &str,
        git_ref: Option<&str>,
        token: Option<&str>,
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.provision_repo(logical, url, git_ref, token),
            Self::Namespace(sandbox) => sandbox.provision_repo(logical, url, git_ref, token),
            Self::Container { .. } => Err(pc::SandboxError::new(
                "container repository provisioning must be staged before environment creation",
            )),
        }
    }

    pub(crate) fn push_repo(
        &self,
        logical: &str,
        token: Option<&str>,
    ) -> Result<bool, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.push_repo(logical, token),
            Self::Namespace(sandbox) => sandbox.push_repo(logical, token),
            Self::Container { .. } => Err(pc::SandboxError::new(
                "container repository harvest requires a staged checkout volume",
            )),
        }
    }

    pub(crate) fn list_files(&self, subdir: &str) -> Vec<(String, Vec<u8>)> {
        match self {
            Self::Workdir(sandbox) => sandbox.list_files(subdir),
            Self::Namespace(sandbox) => sandbox.list_files(subdir),
            Self::Container { .. } => Vec::new(),
        }
    }

    pub(crate) fn scan_skill_dir(&self, subdir: &str) -> Vec<DiscoveredSkillFile> {
        match self {
            Self::Workdir(sandbox) => sandbox.scan_skill_dir(subdir),
            Self::Namespace(sandbox) => sandbox.scan_skill_dir(subdir),
            Self::Container { .. } => Vec::new(),
        }
    }

    pub(crate) async fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.materialize_inline(logical, contents),
            Self::Namespace(sandbox) => sandbox.materialize_inline(logical, contents),
            Self::Container { sandbox, .. } => {
                if !logical.starts_with('/')
                    || logical.split('/').any(|part| part == "." || part == "..")
                {
                    return Err(pc::SandboxError::new(
                        "unsafe container materialization path",
                    ));
                }
                let mut writer = sandbox
                    .spawn_agent_process(pc::Command {
                        argv: vec![
                            "sh".into(),
                            "-c".into(),
                            "umask 077; mkdir -p -- \"$(dirname -- \"$1\")\" && cat > \"$1\""
                                .into(),
                            "awaken-materialize".into(),
                            logical.to_string(),
                        ],
                        cwd: "/workspace".into(),
                        env: Vec::new(),
                        stdio: pc::Stdio::Piped,
                    })
                    .await?;
                use tokio::io::AsyncWriteExt;
                writer
                    .channel
                    .write_all(contents)
                    .await
                    .map_err(|error| pc::SandboxError::new(error.to_string()))?;
                writer
                    .channel
                    .shutdown()
                    .await
                    .map_err(|error| pc::SandboxError::new(error.to_string()))?;
                let status = writer.process.wait().await?;
                if status.code == Some(0) {
                    Ok(())
                } else {
                    Err(pc::SandboxError::new(format!(
                        "container materialization exited {:?}",
                        status.code
                    )))
                }
            }
        }
    }

    pub(crate) async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<(Box<dyn pc::ProcessHandle>, Box<dyn AgentChannelType>), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.spawn_agent(command).await,
            Self::Namespace(sandbox) => sandbox.spawn_agent(command).await,
            Self::Container { sandbox, .. } => sandbox
                .spawn_agent_process(command)
                .await
                .map(|process| (process.process, process.channel)),
        }
    }
}

#[async_trait]
impl AgentSandbox for SessionEnvironment {
    fn is_container(&self) -> bool {
        matches!(self, Self::Container { .. })
    }

    fn config_home(&self) -> &'static str {
        match self {
            Self::Container { .. } => "/acp-config",
            Self::Workdir(_) | Self::Namespace(_) => ".acp-config",
        }
    }

    async fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        self.materialize_inline(logical, contents).await
    }

    async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<
        (
            Box<dyn pc::ProcessHandle>,
            Box<dyn awaken_run_executor_acp::AgentChannelType>,
        ),
        pc::SandboxError,
    > {
        self.spawn_agent(command).await
    }
}

#[async_trait]
impl pc::Sandbox for SessionEnvironment {
    fn id(&self) -> &str {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::id(sandbox.as_ref()),
            Self::Namespace(sandbox) => pc::Sandbox::id(sandbox.as_ref()),
            Self::Container { sandbox, .. } => sandbox.id(),
        }
    }

    fn handle(&self) -> pc::SandboxHandle {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::handle(sandbox.as_ref()),
            Self::Namespace(sandbox) => pc::Sandbox::handle(sandbox.as_ref()),
            Self::Container { sandbox, .. } => sandbox.handle(),
        }
    }

    async fn spawn(
        &self,
        command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::spawn(sandbox.as_ref(), command).await,
            Self::Namespace(sandbox) => pc::Sandbox::spawn(sandbox.as_ref(), command).await,
            Self::Container { sandbox, .. } => sandbox.spawn(command).await,
        }
    }

    async fn attach(
        &self,
        requirement: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::attach(sandbox.as_ref(), requirement).await,
            Self::Namespace(sandbox) => pc::Sandbox::attach(sandbox.as_ref(), requirement).await,
            Self::Container { sandbox, .. } => sandbox.attach(requirement).await,
        }
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::artifacts(sandbox.as_ref()).await,
            Self::Namespace(sandbox) => pc::Sandbox::artifacts(sandbox.as_ref()).await,
            Self::Container { sandbox, .. } => sandbox.artifacts().await,
        }
    }

    async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::read_artifact(sandbox.as_ref(), id).await,
            Self::Namespace(sandbox) => pc::Sandbox::read_artifact(sandbox.as_ref(), id).await,
            Self::Container { sandbox, .. } => sandbox.read_artifact(id).await,
        }
    }

    fn realized(&self) -> &[pc::RealizedMount] {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::realized(sandbox.as_ref()),
            Self::Namespace(sandbox) => pc::Sandbox::realized(sandbox.as_ref()),
            Self::Container { sandbox, .. } => sandbox.realized(),
        }
    }

    async fn process(
        &self,
        process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::process(sandbox.as_ref(), process_id).await,
            Self::Namespace(sandbox) => pc::Sandbox::process(sandbox.as_ref(), process_id).await,
            Self::Container { sandbox, .. } => sandbox.process(process_id).await,
        }
    }

    async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::status(sandbox.as_ref()).await,
            Self::Namespace(sandbox) => pc::Sandbox::status(sandbox.as_ref()).await,
            Self::Container { sandbox, .. } => sandbox.status().await,
        }
    }

    async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::renew_lease(sandbox.as_ref()).await,
            Self::Namespace(sandbox) => pc::Sandbox::renew_lease(sandbox.as_ref()).await,
            Self::Container { sandbox, .. } => sandbox.renew_lease().await,
        }
    }

    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::dispose(sandbox.as_ref()).await,
            Self::Namespace(sandbox) => pc::Sandbox::dispose(sandbox.as_ref()).await,
            Self::Container {
                sandbox,
                hand_process,
                ..
            } => {
                let _ = hand_process.signal(pc::Signal::Term).await;
                let _ = hand_process.wait().await;
                sandbox.dispose().await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_provisioning_contract::{
        IsolationClass, NetworkPolicy, ResourceLimits, Sandbox, SandboxSpec,
    };
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_sandbox_local::{LocalProvider, NamespaceProvider};
    use tokio::io::AsyncReadExt;

    #[derive(Default)]
    struct FakeContainerProvider {
        creates: std::sync::atomic::AtomicUsize,
        shared: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    }

    struct FakeContainer {
        shared: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    }

    struct DoneProcess(String);

    struct FakeHandExecutorFactory;

    struct FakeHandExecutor;

    impl HandExecutorFactory for FakeHandExecutorFactory {
        fn bind(
            &self,
            _channel: Box<dyn AgentChannelType>,
            _operation_scope: &str,
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
            &self.0
        }

        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            Ok(pc::ExitStatus {
                code: Some(0),
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
    impl awaken_sandbox_container::ContainerEnvironmentProvider for FakeContainerProvider {
        async fn create_environment(
            &self,
            _spec: &pc::SandboxSpec,
        ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError>
        {
            self.creates
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Arc::new(FakeContainer {
                shared: self.shared.clone(),
            }))
        }

        async fn adopt_environment(
            &self,
            _handle: &pc::SandboxHandle,
        ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError>
        {
            Ok(Arc::new(FakeContainer {
                shared: self.shared.clone(),
            }))
        }
    }

    #[async_trait]
    impl awaken_sandbox_container::ContainerEnvironment for FakeContainer {
        async fn spawn_agent_process(
            &self,
            command: pc::Command,
        ) -> Result<awaken_sandbox_container::RuntimeAgentProcess, pc::SandboxError> {
            let (ours, theirs) = tokio::io::duplex(64 * 1024);
            if command.argv.iter().any(|part| part == "--stdio") {
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
            } else {
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut theirs = theirs;
                    let mut bytes = Vec::new();
                    let _ = theirs.read_to_end(&mut bytes).await;
                });
            }
            Ok(awaken_sandbox_container::RuntimeAgentProcess {
                process: Box::new(DoneProcess("container-exec".into())),
                channel: Box::new(ours),
            })
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
            Ok(Box::new(DoneProcess("native-exec".into())))
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
            Ok(Box::new(DoneProcess(id.into())))
        }

        async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
            Ok(pc::SandboxStatus::Ready)
        }

        async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
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
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/outputs".into(),
            limits: ResourceLimits::default(),
            lease_ttl_secs: None,
            extra: None,
        }
    }

    #[tokio::test]
    async fn native_process_and_agent_channel_share_one_live_environment() {
        let base = tempfile::tempdir().unwrap();
        let local = LocalProvider::new(base.path())
            .create_sandbox(&spec())
            .await
            .unwrap();
        let original = Sandbox::handle(&local);
        let environment = SessionEnvironment::workdir(local);
        assert_eq!(Sandbox::handle(&environment), original);

        let native = environment
            .spawn(pc::Command::new([
                "/bin/sh",
                "-c",
                "printf shared-state > marker",
            ]))
            .await
            .unwrap();
        assert_eq!(native.wait().await.unwrap().code, Some(0));

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
        let base = tempfile::tempdir().unwrap();
        let mut namespace_spec = spec();
        namespace_spec.scope = "session-namespace".into();
        namespace_spec.isolation = IsolationClass::Namespace;
        let namespace = NamespaceProvider::new(base.path())
            .create_sandbox(&namespace_spec)
            .await
            .unwrap();
        let environment = SessionEnvironment::namespace(namespace);
        assert_eq!(environment.handle().provider_kind, "bwrap");

        let mut native_command =
            pc::Command::new(["/bin/sh", "-c", "printf namespace-state > marker"]);
        native_command.cwd = "/workspace".into();
        let native = environment.spawn(native_command).await.unwrap();
        assert_eq!(native.wait().await.unwrap().code, Some(0));

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
        )
        .create(&spec())
        .await
        .unwrap();
        assert_eq!(
            provider.creates.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(environment.handle().provider_kind, "container");

        let native = environment
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

        let hand = environment.bound_tool_executor().expect("container hand");
        let result = hand
            .invoke(&ToolCall {
                call_id: "bound-hand".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({"command": "printf bound-hand-ok"}),
            })
            .await
            .unwrap();
        assert!(result.content.contains("bound-hand-ok"));
        environment.dispose().await.unwrap();
    }
}
